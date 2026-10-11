//! The WebRTC thread of a web session: str0m driven by the socket, the
//! session's output and its timers.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, bounded, select};
use fernsicht_input::InputSink;
use fernsicht_proto::{Codec, InputEvent};
use str0m::channel::ChannelId;
use str0m::format::Codec as RtcCodec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};

use super::commands::{Command, newer, parse_input};
use super::offer::WebSetup;
use super::{CONNECT_WITHIN, WebOut};
use crate::shared::Shared;

/// Starts the WebRTC thread of a session. It ends when the browser goes
/// away (then it clears `shared.running`, which ends the session) or the
/// session ends.
pub(crate) fn spawn(
    setup: WebSetup,
    codec: Codec,
    shared: Arc<Shared>,
    out: Receiver<WebOut>,
    input: Option<Box<dyn InputSink>>,
) -> std::io::Result<JoinHandle<()>> {
    // The browser's picture leaves and its input arrives on this thread.
    fernsicht_core::thread::spawn_hot("webrtc", move || {
        let mut s = RtcSession::new(setup, codec, shared.clone(), input);
        if let Err(e) = s.run(&out) {
            log::warn!("web session: {e:#}");
        }
        s.end();
    })
}

struct RtcSession {
    rtc: Rtc,
    socket: UdpSocket,
    shared: Arc<Shared>,
    input: Option<Box<dyn InputSink>>,
    connected: bool,
    /// What the encoder makes: H.264 or AV1.
    codec: Codec,
    video: Option<(Mid, Pt)>,
    audio: Option<(Mid, Pt)>,
    /// The reliable channel: input, and our pointer and stats out.
    channel: Option<ChannelId>,
    /// Number of the last pointer position applied (see `Command::Position`).
    last_position: Option<u32>,
    /// RTP time of the next Opus frame.
    audio_time: u64,
    /// The round trip the browser's receiver reports tell.
    rtt: Duration,
}

impl RtcSession {
    pub(crate) fn new(
        setup: WebSetup,
        codec: Codec,
        shared: Arc<Shared>,
        input: Option<Box<dyn InputSink>>,
    ) -> Self {
        Self {
            rtc: setup.rtc,
            socket: setup.socket,
            shared,
            input,
            connected: false,
            codec,
            video: None,
            audio: None,
            channel: None,
            last_position: None,
            audio_time: 0,
            rtt: Duration::ZERO,
        }
    }

    fn run(&mut self, out: &Receiver<WebOut>) -> anyhow::Result<()> {
        let local = self.socket.local_addr()?;
        let net = receive_thread(self.socket.try_clone()?, self.shared.clone())?;
        let started = Instant::now();
        let result = (|| {
            while self.shared.running.load(Ordering::Acquire) {
                if !self.connected && started.elapsed() > CONNECT_WITHIN {
                    log::info!("web session: the browser never connected");
                    return Ok(());
                }
                let deadline = match self.poll()? {
                    Some(t) => t,
                    None => return Ok(()),
                };
                let now = Instant::now();
                if deadline <= now {
                    self.rtc.handle_input(Input::Timeout(now))?;
                    continue;
                }
                let wait = (deadline - now).min(Duration::from_millis(100));
                select! {
                    recv(net.1) -> m => {
                        let Ok((at, source, data)) = m else { return Ok(()) };
                        let Ok(contents) = data.as_slice().try_into() else { continue };
                        self.rtc.handle_input(Input::Receive(at, Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: local,
                            contents,
                        }))?;
                    }
                    recv(out) -> m => match m {
                        Ok(m) => self.send(m),
                        // The session's threads are gone.
                        Err(_) => return Ok(()),
                    },
                    default(wait) => self.rtc.handle_input(Input::Timeout(Instant::now()))?,
                }
            }
            Ok(())
        })();
        self.shared.running.store(false, Ordering::Release);
        let _ = net.0.join();
        result
    }

    /// Sends what str0m has to send and handles its events; returns when
    /// it wants to be woken next, `None` when the browser is gone.
    fn poll(&mut self) -> anyhow::Result<Option<Instant>> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(t) => return Ok(Some(t)),
                Output::Transmit(t) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Output::Event(e) => {
                    if !self.on_event(e) {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// Returns false when the session is over.
    fn on_event(&mut self, e: Event) -> bool {
        match e {
            Event::Connected => {
                log::info!("web session: browser connected");
                self.connected = true;
                self.shared
                    .keyframe_requested
                    .store(true, Ordering::Relaxed);
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                log::info!("web session: browser gone");
                return false;
            }
            Event::MediaAdded(m) => {
                let Some(writer) = self.rtc.writer(m.mid) else {
                    return true;
                };
                let params: Vec<_> = writer.payload_params().cloned().collect();
                match m.kind {
                    MediaKind::Video => {
                        self.video = pick_video(&params, self.codec).map(|pt| (m.mid, pt));
                    }
                    MediaKind::Audio => {
                        self.audio = params
                            .iter()
                            .find(|p| p.spec().codec == RtcCodec::Opus)
                            .map(|p| (m.mid, p.pt()));
                    }
                }
            }
            // Browsers ask (PLI) until the keyframe arrives: one answers
            // them all.
            Event::KeyframeRequest(_) => self.shared.want_keyframe(None, self.rtt),
            Event::MediaEgressStats(s) => {
                if let Some(rtt) = s.rtt {
                    self.rtt = rtt;
                }
            }
            // "moves" brings pointer moves only (unordered, no retransmits);
            // the other one carries everything else, both ways.
            Event::ChannelOpen(_, label) if label == "moves" => {}
            Event::ChannelOpen(id, _) => {
                self.channel = Some(id);
                self.shared.cursor_requested.store(true, Ordering::Relaxed);
            }
            Event::ChannelData(d) if !d.binary => match parse_input(&d.data) {
                Some(Command::Event(event)) => self.inject(&event),
                // Over two channels, one unordered: an older position must
                // not undo a newer one.
                Some(Command::Position(event, seq)) => {
                    if self.last_position.is_none_or(|last| newer(seq, last)) {
                        self.last_position = Some(seq);
                        self.inject(&event);
                    }
                }
                Some(Command::ReleaseAll) => {
                    if let Some(sink) = self.input.as_mut() {
                        sink.release_all();
                    }
                }
                Some(Command::Keyframe) => self.shared.want_keyframe(None, self.rtt),
                Some(Command::Monitor(i)) => self.shared.select_monitor(i),
                Some(Command::Bye) => {
                    log::info!("web session: browser said goodbye");
                    return false;
                }
                None => log::debug!("web input: unreadable message"),
            },
            // The page went away (closed, reloaded): no need to wait for
            // the connection to time out.
            Event::ChannelClose(_) => {
                log::info!("web session: data channel closed");
                return false;
            }
            _ => {}
        }
        true
    }

    pub(crate) fn send(&mut self, m: WebOut) {
        if !self.connected {
            return;
        }
        match m {
            WebOut::Video { data, capture_us } => {
                let Some((mid, pt)) = self.video else { return };
                if let Some(w) = self.rtc.writer(mid) {
                    let time = MediaTime::new(capture_us * 9 / 100, Frequency::NINETY_KHZ);
                    if let Err(e) = w.write(pt, Instant::now(), time, data) {
                        log::debug!("web video: {e}");
                    }
                }
            }
            WebOut::Audio { data } => {
                let Some((mid, pt)) = self.audio else { return };
                if let Some(w) = self.rtc.writer(mid) {
                    let time = MediaTime::new(self.audio_time, Frequency::FORTY_EIGHT_KHZ);
                    let _ = w.write(pt, Instant::now(), time, data);
                }
                self.audio_time += fernsicht_audio::FRAME_SAMPLES as u64;
            }
            WebOut::Packet(p) => self.write_channel(true, &p),
            WebOut::Text(t) => self.write_channel(false, t.as_bytes()),
        }
    }

    fn inject(&mut self, event: &InputEvent) {
        if let Some(sink) = self.input.as_mut()
            && let Err(e) = sink.inject(event)
        {
            log::debug!("web input: {e}");
        }
    }

    fn write_channel(&mut self, binary: bool, data: &[u8]) {
        if let Some(mut c) = self.channel.and_then(|id| self.rtc.channel(id)) {
            // A full buffer drops the message: the pointer is sent again
            // with the next frame, stats every second.
            let _ = c.write(binary, data);
        }
    }

    fn end(&mut self) {
        if let Some(mut sink) = self.input.take() {
            sink.release_all();
        }
        self.rtc.disconnect();
        self.shared.running.store(false, Ordering::Release);
    }
}

/// Datagrams from the socket, read on their own thread so that the WebRTC
/// thread can wait on them and on the session's output at once.
type Datagram = (Instant, SocketAddr, Vec<u8>);

fn receive_thread(
    socket: UdpSocket,
    shared: Arc<Shared>,
) -> std::io::Result<(JoinHandle<()>, Receiver<Datagram>)> {
    let (tx, rx) = bounded::<Datagram>(256);
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let t = fernsicht_core::thread::spawn_hot("webrtc-recv", move || {
        let mut buf = vec![0u8; 2048];
        while shared.running.load(Ordering::Acquire) {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if tx.send((Instant::now(), from, buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => {
                    log::debug!("web socket: {e}");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    })?;
    Ok((t, rx))
}

/// The payload type to send `codec` with. For H.264: High if the browser
/// takes it, else any packetization-mode-1 H.264.
fn pick_video(params: &[str0m::format::PayloadParams], codec: Codec) -> Option<Pt> {
    if codec == Codec::Av1 {
        return params
            .iter()
            .find(|p| p.spec().codec == RtcCodec::Av1)
            .map(|p| p.pt());
    }
    let h264 = |p: &&str0m::format::PayloadParams| {
        let s = p.spec();
        s.codec == RtcCodec::H264 && s.format.packetization_mode == Some(1)
    };
    let profile =
        |p: &str0m::format::PayloadParams| p.spec().format.profile_level_id.map(|v| v >> 16);
    params
        .iter()
        .filter(h264)
        .find(|p| profile(p) == Some(0x64))
        .or_else(|| params.iter().find(h264))
        .map(|p| p.pt())
}
