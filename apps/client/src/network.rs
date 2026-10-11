//! The network thread: receiving and reassembling video, clock sync,
//! feedback, keyframe requests and the overlay's numbers.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use fernsicht_audio::jitter::Encoded;
use fernsicht_core::now_us;
use fernsicht_net::keyframe::Asks;
use fernsicht_net::{ClockSync, LossSim, Reassembler, ReceiverStats, keyframe};
use fernsicht_proto::{
    Bye, ClockPing, Codec, Feedback, Handshake, Hello, HelloAck, InputHeader, KeyframeRequest,
    MAX_DATAGRAM, Packet, SealedHeader, SelectMonitor, VideoHeader,
};
use fernsicht_render::overlay;
use fernsicht_render::overlay::StreamInfo;
use fernsicht_secure::session::{Initiator, Transport};

use crate::audio::AudioPacket;
use crate::config::{ClientConfig, CodecChoice, offered_codecs};
use crate::cursor::CursorTracker;
use crate::hosts::reject_text;
use crate::input::InputLink;
use crate::monitors::{Monitor, MonitorList};
use crate::present::Hw;

pub(crate) const HELLO_INTERVAL: Duration = Duration::from_millis(250);

const FEEDBACK_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) const OVERLAY_INTERVAL: Duration = Duration::from_secs(1);

/// Warn when the host acked but no video arrived for this long.
const NO_VIDEO_WARNING: Duration = Duration::from_secs(2);

/// How often the goodbye is sent at the end of a session.
const BYE_REPEATS: usize = 3;

/// Sends `packet` to the host: sealed once the session is secure.
pub(crate) fn send_packet(socket: &UdpSocket, crypto: Option<(&Transport, u32)>, packet: &[u8]) {
    let Some((t, session_id)) = crypto else {
        let _ = socket.send(packet);
        return;
    };
    let mut buf = [0u8; MAX_DATAGRAM];
    if let Ok((counter, n)) = t.seal(packet, &mut buf[SealedHeader::LEN..]) {
        SealedHeader {
            session_id,
            counter,
        }
        .write(&mut buf);
        let _ = socket.send(&buf[..SealedHeader::LEN + n]);
    }
}

/// What the network thread learned during the run.
pub(crate) struct NetOutcome {
    pub(crate) receiver: ReceiverStats,
    pub(crate) overflowed: u64,
    pub(crate) session: Option<(u32, Codec)>,
    pub(crate) clock_offset_us: i64,
    pub(crate) rtt_us: Option<u64>,
}

/// A reassembled frame on its way to the decoder.
pub(crate) struct ReceivedFrame {
    pub(crate) header: VideoHeader,
    /// Codec announced by the host for this session.
    pub(crate) codec: Codec,
    pub(crate) data: Vec<u8>,
    pub(crate) completed_us: u64,
    /// Host clock − client clock at the time the frame completed.
    pub(crate) offset_us: i64,
}

impl ReceivedFrame {
    pub(crate) fn empty() -> Self {
        Self {
            header: VideoHeader::default(),
            codec: Codec::Synthetic,
            data: Vec::new(),
            completed_us: 0,
            offset_us: 0,
        }
    }
}

/// The network thread's ends of the decode queue.
pub(crate) struct Pipe<'a> {
    /// `None` without sound output.
    pub(crate) audio_tx: Option<&'a Sender<AudioPacket>>,
    pub(crate) decode_tx: &'a Sender<ReceivedFrame>,
    pub(crate) free_rx: &'a Receiver<ReceivedFrame>,
    pub(crate) free_tx: &'a Sender<ReceivedFrame>,
    /// Set by the decoder while its reference chain is broken.
    pub(crate) need_keyframe: &'a AtomicBool,
    pub(crate) cursor: &'a Mutex<CursorTracker>,
}

/// The network thread's state between datagrams.
struct Net {
    hello: Hello,
    reassembler: Reassembler,
    clock: ClockSync,
    loss: LossSim,
    /// The session and its codec, once the host acked.
    session: Option<(u32, Codec)>,
    /// The host's frame interval, from its HelloAck.
    frame_interval: Duration,
    keyframe_asks: Asks,
    last_hello: Option<Instant>,
    last_ping: Option<Instant>,
    ping_seq: u32,
    last_feedback: Instant,
    last_packet: Instant,
    rates: RateWindow,
    overflowed: u64,
    acked_at: Option<Instant>,
    video_seen: bool,
    /// Secure: the handshake in flight, then the session's keys.
    handshake: Option<(Initiator, Vec<u8>)>,
    crypto: Option<Arc<Transport>>,
    /// Packets on their way out are encoded here.
    out: [u8; MAX_DATAGRAM],
}

pub(crate) fn network_loop(
    cfg: &ClientConfig,
    socket: &UdpSocket,
    stop: &AtomicBool,
    pipe: &Pipe<'_>,
    info: &Mutex<StreamInfo>,
) -> anyhow::Result<NetOutcome> {
    let deadline = cfg.duration.map(|d| Instant::now() + d);
    let mut net = start(cfg)?;
    // A run of datagrams the kernel coalesced (GRO), or one.
    let mut rx = vec![0u8; 65_536];
    let mut opened = [0u8; MAX_DATAGRAM];

    'session: loop {
        // Acquire: what the window queued before it set `stop` (the keys it
        // released on closing) is seen below.
        if stop.load(Ordering::Acquire) || deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        check_host(cfg, &mut net)?;
        send_due(cfg, socket, pipe, info, &mut net);
        let Some((len, size)) = receive(socket, &mut rx)? else {
            continue;
        };
        let now = now_us();
        // With GRO a run of datagrams comes at once.
        for datagram in rx[..len].chunks(size) {
            let Ok(outer) = Packet::decode(datagram) else {
                continue;
            };
            let Some(packet) = authentic(cfg, &mut net, outer, &mut opened)? else {
                continue;
            };
            if !on_packet(cfg, socket, pipe, &mut net, packet, datagram.len(), now)? {
                break 'session;
            }
        }
    }

    say_goodbye(cfg, socket, &mut net);
    Ok(NetOutcome {
        receiver: net.reassembler.totals(),
        overflowed: net.overflowed,
        session: net.session,
        clock_offset_us: net.clock.offset_at(now_us()),
        rtt_us: net.clock.rtt_us(),
    })
}

/// The hello for `cfg` (the codecs this machine decodes) and, for a secure
/// session, the handshake that carries it.
fn start(cfg: &ClientConfig) -> anyhow::Result<Net> {
    let hw = Hw {
        render_node: cfg.render_node.clone(),
        decoder: cfg.decoder,
    };
    let codecs = offered_codecs(cfg.codec, |c| hw.decodes(c));
    log::debug!("offering {codecs:?}");
    let forced = match cfg.codec {
        CodecChoice::Hevc => Some(Codec::Hevc),
        CodecChoice::Av1 => Some(Codec::Av1),
        _ => None,
    };
    if let Some(c) = forced.filter(|&c| !hw.decodes(c)) {
        log::warn!(
            "{} asked for, but no hardware decoder here reports support for it",
            overlay::codec_label(c)
        );
    }
    let hello = Hello {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        bitrate_kbps: cfg.bitrate_kbps,
        codecs,
    };
    let handshake = match &cfg.security {
        Some(sec) => {
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            let payload = hello.encode_with_time(unix_ms);
            Some(
                Initiator::start(&sec.identity, &sec.host, &payload)
                    .map_err(|e| anyhow::anyhow!("handshake: {e}"))?,
            )
        }
        None => None,
    };
    Ok(Net {
        hello,
        reassembler: Reassembler::new(),
        clock: ClockSync::new(),
        loss: LossSim::new(cfg.loss, 0xF00D),
        session: None,
        frame_interval: Duration::from_millis(16),
        keyframe_asks: Asks::default(),
        last_hello: None,
        last_ping: None,
        ping_seq: 0,
        last_feedback: Instant::now(),
        last_packet: Instant::now(),
        rates: RateWindow::new(),
        overflowed: 0,
        acked_at: None,
        video_seen: false,
        handshake,
        crypto: None,
        out: [0; MAX_DATAGRAM],
    })
}

/// Warns once when the host acked but sends no video; fails when it has
/// been silent for longer than `cfg.host_timeout`.
fn check_host(cfg: &ClientConfig, net: &mut Net) -> anyhow::Result<()> {
    if !net.video_seen
        && net
            .acked_at
            .is_some_and(|t| t.elapsed() >= NO_VIDEO_WARNING)
    {
        log::warn!(
            "the host accepted the session but sends no video for {NO_VIDEO_WARNING:?}; \
             the host agent's log says why"
        );
        net.video_seen = true; // warn once
    }
    if net.last_packet.elapsed() > cfg.host_timeout {
        anyhow::bail!("no packets from {} for {:?}", cfg.host, cfg.host_timeout);
    }
    Ok(())
}

/// The keys packets to the host are sealed with, once the session is secure.
fn seal_with(net: &Net) -> Option<(&Transport, u32)> {
    net.crypto.as_deref().zip(net.session.map(|(id, _)| id))
}

/// Sends what is due: the hello until the host answers, then input, a
/// keyframe request, clock pings, a monitor switch and feedback.
fn send_due(
    cfg: &ClientConfig,
    socket: &UdpSocket,
    pipe: &Pipe<'_>,
    info: &Mutex<StreamInfo>,
    net: &mut Net,
) {
    send_hello(socket, net);
    if let Some((session_id, _)) = net.session {
        send_input(cfg, socket, net, session_id);
        ask_keyframe(socket, pipe, net, session_id);
    }
    ping(cfg, socket, net);
    if let Some((session_id, codec)) = net.session {
        ask_monitor(cfg, socket, net, session_id);
        send_feedback(socket, pipe, info, net, session_id, codec);
    }
}

fn send_hello(socket: &UdpSocket, net: &mut Net) {
    if net.session.is_some() || net.last_hello.is_some_and(|t| t.elapsed() < HELLO_INTERVAL) {
        return;
    }
    let n = match &net.handshake {
        Some((_, msg1)) => Handshake {
            reply: false,
            message: msg1,
        }
        .encode(&mut net.out),
        None => net.hello.encode(&mut net.out),
    };
    let _ = socket.send(&net.out[..n]);
    net.last_hello = Some(Instant::now());
}

/// Input the queue has due (new, or not acked in time).
fn send_input(cfg: &ClientConfig, socket: &UdpSocket, net: &mut Net, session_id: u32) {
    let Some(input) = &cfg.input else { return };
    let n = input.queue.lock().ok().and_then(|mut q| {
        q.due(Instant::now())
            .map(|events| InputHeader::encode(session_id, events, &mut net.out))
    });
    if let Some(n) = n {
        send_packet(socket, seal_with(net), &net.out[..n]);
    }
}

/// A broken picture asks for a keyframe at once, not at the next feedback
/// tick, and again only when the answer should have come.
fn ask_keyframe(socket: &UdpSocket, pipe: &Pipe<'_>, net: &mut Net, session_id: u32) {
    let needed = net.reassembler.needs_keyframe() || pipe.need_keyframe.load(Ordering::Relaxed);
    let rtt = Duration::from_micros(net.clock.rtt_us().unwrap_or(0));
    let wait = keyframe::in_flight_for(rtt, net.frame_interval);
    if !net.keyframe_asks.due(needed, Instant::now(), wait) {
        return;
    }
    let ask = KeyframeRequest {
        session_id,
        last_complete: net.reassembler.last_completed(),
        rtt_us: u32::try_from(rtt.as_micros()).unwrap_or(u32::MAX),
    };
    let n = ask.encode(&mut net.out);
    send_packet(socket, seal_with(net), &net.out[..n]);
}

/// Pings fast until the clock offset settles, then slowly to track drift.
fn ping(cfg: &ClientConfig, socket: &UdpSocket, net: &mut Net) {
    // A secure host answers sealed pings only, so wait for the session.
    let can_ping = cfg.security.is_none() || seal_with(net).is_some();
    let every = if net.ping_seq < 20 {
        Duration::from_millis(50)
    } else {
        Duration::from_millis(500)
    };
    if !can_ping || net.last_ping.is_some_and(|t| t.elapsed() < every) {
        return;
    }
    let ping = ClockPing {
        seq: net.ping_seq,
        client_send_us: now_us(),
    };
    let n = ping.encode(&mut net.out);
    send_packet(socket, seal_with(net), &net.out[..n]);
    net.ping_seq += 1;
    net.last_ping = Some(Instant::now());
}

/// The monitor the user asked for, if the host listed it.
fn ask_monitor(cfg: &ClientConfig, socket: &UdpSocket, net: &mut Net, session_id: u32) {
    let Some(request) = cfg.monitors.take_request() else {
        return;
    };
    match cfg.monitors.list().and_then(|l| l.resolve(request)) {
        Some(index) => {
            let n = SelectMonitor { session_id, index }.encode(&mut net.out);
            send_packet(socket, seal_with(net), &net.out[..n]);
            log::info!("asked the host for monitor {index}");
        }
        None => log::info!("no monitor for {request:?} (the host listed none or fewer)"),
    }
}

/// What arrived since the last feedback, and the overlay's numbers once a
/// window is full.
fn send_feedback(
    socket: &UdpSocket,
    pipe: &Pipe<'_>,
    info: &Mutex<StreamInfo>,
    net: &mut Net,
    session_id: u32,
    codec: Codec,
) {
    if net.last_feedback.elapsed() < FEEDBACK_INTERVAL {
        return;
    }
    let s = net.reassembler.take_interval();
    let fb = Feedback {
        session_id,
        request_keyframe: net.reassembler.needs_keyframe()
            || pipe.need_keyframe.load(Ordering::Relaxed),
        request_cursor: pipe.cursor.lock().is_ok_and(|t| t.needs_shape()),
        highest_frame_id: s.highest_frame_id,
        frames_completed: s.frames_completed,
        frames_dropped: s.frames_dropped,
        packets_received: s.packets_received,
        packets_lost: s.packets_lost,
        packets_recovered: s.packets_recovered,
    };
    let n = fb.encode(&mut net.out);
    send_packet(socket, seal_with(net), &net.out[..n]);
    net.rates.add(&s);
    if net.rates.started.elapsed() >= OVERLAY_INTERVAL
        && let Ok(mut i) = info.lock()
    {
        net.rates.publish(&mut i, codec, net.clock.rtt_us());
        net.rates = RateWindow::new();
    }
    net.last_feedback = Instant::now();
}

/// The next datagrams (a run of one size with GRO): their length and the
/// size of each; `None` when the wait ran out or the host is not up yet.
fn receive(socket: &UdpSocket, rx: &mut [u8]) -> anyhow::Result<Option<(usize, usize)>> {
    match fernsicht_net::socket::recv_segments(socket, rx) {
        Ok(n) => Ok(Some(n)),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(None)
        }
        // ICMP port unreachable while the host isn't up yet.
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(None),
        Err(e) => Err(e).context("recv"),
    }
}

/// The packet as far as it comes from the host. Only that keeps the session
/// alive: in a secure session the sealed packets (opened into `opened`) and
/// the handshake's answer, else any packet.
fn authentic<'a>(
    cfg: &ClientConfig,
    net: &mut Net,
    outer: Packet<'a>,
    opened: &'a mut [u8],
) -> anyhow::Result<Option<Packet<'a>>> {
    if cfg.security.is_none() {
        net.last_packet = Instant::now();
        return Ok(Some(outer));
    }
    match outer {
        Packet::Sealed(h, sealed) => {
            let (Some(t), Some((id, _))) = (net.crypto.as_deref(), net.session) else {
                return Ok(None);
            };
            if h.session_id != id {
                return Ok(None);
            }
            let Ok(n) = t.open(h.counter, sealed, opened) else {
                return Ok(None);
            };
            let opened: &'a [u8] = opened;
            let Ok(inner) = Packet::decode(&opened[..n]) else {
                return Ok(None);
            };
            net.last_packet = Instant::now();
            Ok(Some(inner))
        }
        Packet::Handshake(h) if h.reply => {
            let Some((initiator, _)) = net.handshake.as_mut() else {
                return Ok(None);
            };
            // A reply that does not verify is someone else's: wait for the
            // host's.
            let payload = match initiator.read_reply(h.message) {
                Ok(p) => p,
                Err(e) => {
                    log::debug!("{}: ignoring a handshake answer: {e}", cfg.host);
                    return Ok(None);
                }
            };
            let (initiator, _) = net.handshake.take().expect("checked above");
            let transport = initiator
                .into_transport()
                .map_err(|e| anyhow::anyhow!("handshake with {} failed: {e}", cfg.host))?;
            net.last_packet = Instant::now();
            let Ok(Packet::HelloAck(ack)) = Packet::decode(&payload) else {
                anyhow::bail!("handshake with {}: no session in the answer", cfg.host);
            };
            net.crypto = Some(Arc::new(transport));
            Ok(Some(Packet::HelloAck(ack)))
        }
        // Not authenticated: believed only while asking for a session, never
        // during one (anyone could end it with five bytes).
        Packet::Reject(r) if net.session.is_none() => {
            anyhow::bail!("{}: {}", cfg.host, reject_text(r))
        }
        _ => Ok(None),
    }
}

/// Handles one packet from the host; `false` when the host ended the
/// session.
fn on_packet(
    cfg: &ClientConfig,
    socket: &UdpSocket,
    pipe: &Pipe<'_>,
    net: &mut Net,
    packet: Packet<'_>,
    datagram_len: usize,
    now: u64,
) -> anyhow::Result<bool> {
    let ours = |id: u32| net.session.is_some_and(|(s, _)| s == id);
    match packet {
        // Plain mode (tests): the host could not start the session.
        Packet::Reject(r) if net.session.is_none() => {
            anyhow::bail!("{}: {}", cfg.host, reject_text(r))
        }
        Packet::HelloAck(ack) => on_hello_ack(cfg, socket, net, &ack),
        Packet::ClockPong(pong) => {
            net.clock.on_pong(&pong, now);
            // Lost input goes again after a few round trips.
            if let (Some(input), Some(rtt)) = (&cfg.input, net.clock.rtt_us())
                && let Ok(mut q) = input.queue.lock()
            {
                q.set_resend_after(fernsicht_input::resend_wait(Some(Duration::from_micros(
                    rtt,
                ))));
            }
        }
        Packet::InputAck(a) => {
            if ours(a.session_id)
                && let Some(input) = &cfg.input
                && let Ok(mut q) = input.queue.lock()
            {
                q.ack(a.seq);
            }
        }
        Packet::Audio(h, frame, previous) => {
            if ours(h.session_id)
                && let Some(tx) = pipe.audio_tx
                // The parser bounds both; no allocation per frame.
                && let (Some(frame), Some(previous)) =
                    (Encoded::new(frame), Encoded::new(previous))
            {
                // Full only if playback stalls; then dropping is right.
                let _ = tx.try_send(AudioPacket {
                    seq: h.seq,
                    frame,
                    previous,
                    captured_local: h.capture_us as i64 - net.clock.offset_at(now),
                });
            }
        }
        Packet::Monitors(m) => {
            if ours(m.session_id) {
                cfg.monitors.set_list(MonitorList {
                    current: m.current,
                    list: m
                        .iter()
                        .map(|e| Monitor {
                            name: e.name.to_string(),
                            width: e.width,
                            height: e.height,
                        })
                        .collect(),
                });
            }
        }
        Packet::Cursor(c) => {
            if ours(c.session_id)
                && let Ok(mut t) = pipe.cursor.lock()
            {
                t.on_position(c);
            }
        }
        Packet::CursorShape(s, data) => {
            if ours(s.session_id)
                && let Ok(mut t) = pipe.cursor.lock()
            {
                t.on_shape(&s, data);
            }
        }
        Packet::Video(h, payload) => {
            if ours(h.session_id) {
                on_video(pipe, net, &h, payload, datagram_len, now);
            }
        }
        Packet::Bye(_) => {
            log::info!("host closed the session");
            return Ok(false);
        }
        _ => {}
    }
    Ok(true)
}

/// The host's answer: the session starts (or, repeated, its stream size
/// changed).
fn on_hello_ack(cfg: &ClientConfig, socket: &UdpSocket, net: &mut Net, ack: &HelloAck) {
    if net.session.is_none() {
        net.acked_at = Some(Instant::now());
        log::info!(
            "session {:08x}: {}x{}@{} {}",
            ack.session_id,
            ack.width,
            ack.height,
            ack.fps,
            overlay::codec_label(ack.codec)
        );
    }
    if let Some(input) = &cfg.input {
        input.set_stream((u32::from(ack.width), u32::from(ack.height)));
        if net.session.is_none() {
            // From now on input goes out the moment it comes.
            input.connect(socket.try_clone().ok().map(|s| InputLink {
                socket: Arc::new(s),
                session_id: ack.session_id,
                crypto: net.crypto.clone(),
            }));
        }
    }
    net.session = Some((ack.session_id, ack.codec));
    net.frame_interval = Duration::from_secs(1) / u32::from(ack.fps.max(1));
}

/// One video packet: into the reassembler, and a completed frame on to the
/// decoder.
fn on_video(
    pipe: &Pipe<'_>,
    net: &mut Net,
    h: &VideoHeader,
    payload: &[u8],
    datagram_len: usize,
    now: u64,
) {
    net.video_seen = true;
    if net.loss.drop_packet() {
        return;
    }
    net.rates.bytes += datagram_len as u64;
    let Some(done) = net.reassembler.push(h, payload, now) else {
        return;
    };
    let (header, completed_us) = (done.header, done.completed_us);
    net.rates.frames += 1;
    // The decoder fell behind by a whole queue: drop the frame; the gap
    // makes it ask for a keyframe.
    let Ok(mut rf) = pipe.free_rx.try_recv() else {
        net.overflowed += 1;
        return;
    };
    rf.header = header;
    rf.codec = net.session.map_or(Codec::Synthetic, |(_, codec)| codec);
    // The bytes change hands, no copy.
    if !net
        .reassembler
        .take_completed(header.frame_id, &mut rf.data)
    {
        let _ = pipe.free_tx.send(rf);
        return;
    }
    rf.completed_us = completed_us;
    rf.offset_us = net.clock.offset_at(now);
    if let Err(TrySendError::Full(rf) | TrySendError::Disconnected(rf)) =
        pipe.decode_tx.try_send(rf)
    {
        net.overflowed += 1;
        let _ = pipe.free_tx.send(rf);
    }
}

/// Ends the session on the host: input still due first, then the goodbye.
fn say_goodbye(cfg: &ClientConfig, socket: &UdpSocket, net: &mut Net) {
    // The window may still push; nothing goes out from it any more.
    if let Some(input) = &cfg.input {
        input.connect(None);
    }
    let Some((session_id, _)) = net.session else {
        return;
    };
    // Keys and buttons released on the way out (closing releases what the
    // window held) leave before the goodbye.
    send_input(cfg, socket, net, session_id);
    // More than once: a lost goodbye would keep the session, and anything
    // still pressed, on the host until it times out.
    let n = Bye { session_id }.encode(&mut net.out);
    for _ in 0..BYE_REPEATS {
        send_packet(socket, seal_with(net), &net.out[..n]);
    }
}

/// Accumulates receive rates for the overlay over [`OVERLAY_INTERVAL`], so
/// the numbers don't jump with every 100 ms feedback interval.
pub(crate) struct RateWindow {
    started: Instant,
    pub(crate) bytes: u64,
    pub(crate) frames: u32,
    stats: ReceiverStats,
}

impl RateWindow {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            bytes: 0,
            frames: 0,
            stats: ReceiverStats::default(),
        }
    }

    pub(crate) fn add(&mut self, s: &ReceiverStats) {
        self.stats.packets_received += s.packets_received;
        self.stats.packets_lost += s.packets_lost;
        self.stats.frames_completed += s.frames_completed;
        self.stats.frames_dropped += s.frames_dropped;
    }

    pub(crate) fn publish(&self, info: &mut StreamInfo, codec: Codec, rtt_us: Option<u64>) {
        let secs = self.started.elapsed().as_secs_f32().max(1e-3);
        let s = &self.stats;
        let packets = s.packets_received + s.packets_lost;
        let frames = s.frames_completed + s.frames_dropped;
        info.codec = codec;
        info.fps = self.frames as f32 / secs;
        info.bitrate_bps = (self.bytes as f32 * 8.0 / secs) as u64;
        info.loss_before_fec = ratio(s.packets_lost, packets);
        info.loss_after_fec = ratio(s.frames_dropped, frames);
        info.rtt_us = rtt_us;
    }
}

pub(crate) fn ratio(part: u32, total: u32) -> f32 {
    if total == 0 {
        0.0
    } else {
        part as f32 / total as f32
    }
}
