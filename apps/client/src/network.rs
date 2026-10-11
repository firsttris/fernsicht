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
    Bye, ClockPing, Codec, Feedback, Handshake, Hello, InputHeader, KeyframeRequest, MAX_DATAGRAM,
    Packet, SealedHeader, SelectMonitor, VideoHeader,
};
use fernsicht_render::overlay;
use fernsicht_render::overlay::StreamInfo;
use fernsicht_secure::session::{Initiator, Transport};

use crate::audio::AudioPacket;
use crate::config::{ClientConfig, CodecChoice, offered_codecs};
use crate::cursor::CursorTracker;
use crate::hosts::reject_text;
use crate::input::InputLink;
use crate::monitors::MonitorList;
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

pub(crate) fn network_loop(
    cfg: &ClientConfig,
    socket: &UdpSocket,
    stop: &AtomicBool,
    pipe: &Pipe<'_>,
    info: &Mutex<StreamInfo>,
) -> anyhow::Result<NetOutcome> {
    let started = Instant::now();
    let deadline = cfg.duration.map(|d| started + d);
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
    // A run of datagrams the kernel coalesced (GRO), or one.
    let mut rx = vec![0u8; 65_536];
    let mut out = [0u8; MAX_DATAGRAM];
    let mut reassembler = Reassembler::new();
    let mut clock = ClockSync::new();
    let mut loss = LossSim::new(cfg.loss, 0xF00D);
    let mut session: Option<(u32, Codec)> = None;
    // The host's frame interval, from its HelloAck.
    let mut frame_interval = Duration::from_millis(16);
    let mut keyframe_asks = Asks::default();
    let mut last_hello: Option<Instant> = None;
    let mut last_ping: Option<Instant> = None;
    let mut ping_seq = 0u32;
    let mut last_feedback = Instant::now();
    let mut last_packet = Instant::now();
    let mut rates = RateWindow::new();
    let mut overflowed = 0u64;
    let mut acked_at: Option<Instant> = None;
    let mut video_seen = false;
    let mut opened = [0u8; MAX_DATAGRAM];
    // Secure: the handshake in flight, then the session's keys.
    let mut handshake: Option<(Initiator, Vec<u8>)> = match &cfg.security {
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
    let mut crypto: Option<Arc<Transport>> = None;

    'session: loop {
        // Acquire: what the window queued before it set `stop` (the keys it
        // released on closing) is seen below.
        if stop.load(Ordering::Acquire) || deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        if !video_seen && acked_at.is_some_and(|t| t.elapsed() >= NO_VIDEO_WARNING) {
            log::warn!(
                "the host accepted the session but sends no video for {NO_VIDEO_WARNING:?}; \
                 the host agent's log says why"
            );
            video_seen = true; // warn once
        }
        if last_packet.elapsed() > cfg.host_timeout {
            anyhow::bail!("no packets from {} for {:?}", cfg.host, cfg.host_timeout);
        }
        if session.is_none() && last_hello.is_none_or(|t| t.elapsed() >= HELLO_INTERVAL) {
            let n = match &handshake {
                Some((_, msg1)) => Handshake {
                    reply: false,
                    message: msg1,
                }
                .encode(&mut out),
                None => hello.encode(&mut out),
            };
            let _ = socket.send(&out[..n]);
            last_hello = Some(Instant::now());
        }
        // Sealed with the session's keys once secure.
        let seal_with = crypto.as_deref().zip(session.map(|(id, _)| id));
        // Ping fast until the clock offset settles, then slowly to track drift.
        let ping_every = if ping_seq < 20 {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(500)
        };
        if let (Some((session_id, _)), Some(input)) = (session, &cfg.input) {
            let n = input.queue.lock().ok().and_then(|mut q| {
                q.due(Instant::now())
                    .map(|events| InputHeader::encode(session_id, events, &mut out))
            });
            if let Some(n) = n {
                send_packet(socket, seal_with, &out[..n]);
            }
        }
        // A broken picture asks for a keyframe at once, not at the next
        // feedback tick.
        if let Some((session_id, _)) = session {
            let needed = reassembler.needs_keyframe() || pipe.need_keyframe.load(Ordering::Relaxed);
            let rtt = Duration::from_micros(clock.rtt_us().unwrap_or(0));
            let wait = keyframe::in_flight_for(rtt, frame_interval);
            if keyframe_asks.due(needed, Instant::now(), wait) {
                let ask = KeyframeRequest {
                    session_id,
                    last_complete: reassembler.last_completed(),
                    rtt_us: u32::try_from(rtt.as_micros()).unwrap_or(u32::MAX),
                };
                let n = ask.encode(&mut out);
                send_packet(socket, seal_with, &out[..n]);
            }
        }
        // A secure host answers sealed pings only, so wait for the session.
        let can_ping = cfg.security.is_none() || seal_with.is_some();
        if can_ping && last_ping.is_none_or(|t| t.elapsed() >= ping_every) {
            let ping = ClockPing {
                seq: ping_seq,
                client_send_us: now_us(),
            };
            let n = ping.encode(&mut out);
            send_packet(socket, seal_with, &out[..n]);
            ping_seq += 1;
            last_ping = Some(Instant::now());
        }
        if let Some((session_id, _)) = session
            && let Some(request) = cfg.monitors.take_request()
        {
            match cfg.monitors.list().and_then(|l| l.resolve(request)) {
                Some(index) => {
                    let n = SelectMonitor { session_id, index }.encode(&mut out);
                    send_packet(socket, seal_with, &out[..n]);
                    log::info!("asked the host for monitor {index}");
                }
                None => log::info!("no monitor for {request:?} (the host listed none or fewer)"),
            }
        }
        if let Some((session_id, codec)) = session
            && last_feedback.elapsed() >= FEEDBACK_INTERVAL
        {
            let s = reassembler.take_interval();
            let fb = Feedback {
                session_id,
                request_keyframe: reassembler.needs_keyframe()
                    || pipe.need_keyframe.load(Ordering::Relaxed),
                request_cursor: pipe.cursor.lock().is_ok_and(|t| t.needs_shape()),
                highest_frame_id: s.highest_frame_id,
                frames_completed: s.frames_completed,
                frames_dropped: s.frames_dropped,
                packets_received: s.packets_received,
                packets_lost: s.packets_lost,
                packets_recovered: s.packets_recovered,
            };
            let n = fb.encode(&mut out);
            send_packet(socket, seal_with, &out[..n]);
            rates.add(&s);
            if rates.started.elapsed() >= OVERLAY_INTERVAL
                && let Ok(mut i) = info.lock()
            {
                rates.publish(&mut i, codec, clock.rtt_us());
                rates = RateWindow::new();
            }
            last_feedback = Instant::now();
        }

        let (len, size) = match fernsicht_net::socket::recv_segments(socket, &mut rx) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            // ICMP port unreachable while the host isn't up yet.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
            Err(e) => return Err(e).context("recv"),
        };
        let now = now_us();
        // With GRO a run of datagrams comes at once.
        for datagram in rx[..len].chunks(size) {
            let Ok(outer) = Packet::decode(datagram) else {
                continue;
            };
            // Only what proves to come from the host keeps the session alive:
            // in a secure session the sealed packets and the handshake answer
            // (marked below), else any packet.
            if cfg.security.is_none() {
                last_packet = Instant::now();
            }
            // A secure session: only sealed packets count (opened here), plus
            // the handshake's answer and rejections.
            let packet = match (outer, cfg.security.is_some()) {
                (Packet::Sealed(h, sealed), true) => {
                    let (Some(t), Some((id, _))) = (crypto.as_deref(), session) else {
                        continue;
                    };
                    if h.session_id != id {
                        continue;
                    }
                    let Ok(n) = t.open(h.counter, sealed, &mut opened) else {
                        continue;
                    };
                    let Ok(inner) = Packet::decode(&opened[..n]) else {
                        continue;
                    };
                    last_packet = Instant::now();
                    inner
                }
                (Packet::Handshake(h), true) if h.reply => {
                    let Some((initiator, _)) = handshake.as_mut() else {
                        continue;
                    };
                    // A reply that does not verify is someone else's: wait for
                    // the host's.
                    let payload = match initiator.read_reply(h.message) {
                        Ok(p) => p,
                        Err(e) => {
                            log::debug!("{}: ignoring a handshake answer: {e}", cfg.host);
                            continue;
                        }
                    };
                    let (initiator, _) = handshake.take().expect("checked above");
                    let transport = initiator
                        .into_transport()
                        .map_err(|e| anyhow::anyhow!("handshake with {} failed: {e}", cfg.host))?;
                    last_packet = Instant::now();
                    let Ok(Packet::HelloAck(ack)) = Packet::decode(&payload) else {
                        anyhow::bail!("handshake with {}: no session in the answer", cfg.host);
                    };
                    crypto = Some(Arc::new(transport));
                    Packet::HelloAck(ack)
                }
                // Not authenticated: believed only while asking for a session,
                // never during one (anyone could end it with five bytes).
                (Packet::Reject(r), true) if session.is_none() => {
                    anyhow::bail!("{}: {}", cfg.host, reject_text(r))
                }
                (_, true) => continue,
                (p, false) => p,
            };
            match packet {
                // Plain mode (tests): the host could not start the session.
                Packet::Reject(r) if session.is_none() => {
                    anyhow::bail!("{}: {}", cfg.host, reject_text(r))
                }
                Packet::HelloAck(ack) => {
                    if session.is_none() {
                        acked_at = Some(Instant::now());
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
                        if session.is_none() {
                            // From now on input goes out the moment it comes.
                            input.connect(socket.try_clone().ok().map(|s| InputLink {
                                socket: Arc::new(s),
                                session_id: ack.session_id,
                                crypto: crypto.clone(),
                            }));
                        }
                    }
                    session = Some((ack.session_id, ack.codec));
                    frame_interval = Duration::from_secs(1) / u32::from(ack.fps.max(1));
                }
                Packet::ClockPong(pong) => {
                    clock.on_pong(&pong, now);
                    // Lost input goes again after a few round trips.
                    if let (Some(input), Some(rtt)) = (&cfg.input, clock.rtt_us())
                        && let Ok(mut q) = input.queue.lock()
                    {
                        q.set_resend_after(fernsicht_input::resend_wait(Some(
                            Duration::from_micros(rtt),
                        )));
                    }
                }
                Packet::InputAck(a) => {
                    if session.is_some_and(|(id, _)| id == a.session_id)
                        && let Some(input) = &cfg.input
                        && let Ok(mut q) = input.queue.lock()
                    {
                        q.ack(a.seq);
                    }
                }
                Packet::Audio(h, frame, previous) => {
                    if session.is_some_and(|(id, _)| id == h.session_id)
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
                            captured_local: h.capture_us as i64 - clock.offset_at(now),
                        });
                    }
                }
                Packet::Monitors(m) => {
                    if session.is_some_and(|(id, _)| id == m.session_id) {
                        cfg.monitors.set_list(MonitorList {
                            current: m.current,
                            list: m
                                .iter()
                                .map(|e| (e.name.to_string(), e.width, e.height))
                                .collect(),
                        });
                    }
                }
                Packet::Cursor(c) => {
                    if session.is_some_and(|(id, _)| id == c.session_id)
                        && let Ok(mut t) = pipe.cursor.lock()
                    {
                        t.on_position(c);
                    }
                }
                Packet::CursorShape(s, data) => {
                    if session.is_some_and(|(id, _)| id == s.session_id)
                        && let Ok(mut t) = pipe.cursor.lock()
                    {
                        t.on_shape(&s, data);
                    }
                }
                Packet::Video(h, payload) => {
                    if session.is_none_or(|(id, _)| id != h.session_id) {
                        continue;
                    }
                    video_seen = true;
                    if loss.drop_packet() {
                        continue;
                    }
                    rates.bytes += datagram.len() as u64;
                    if let Some(done) = reassembler.push(&h, payload, now) {
                        let (header, completed_us) = (done.header, done.completed_us);
                        rates.frames += 1;
                        // The decoder fell behind by a whole queue: drop the
                        // frame; the gap makes it ask for a keyframe.
                        let Ok(mut rf) = pipe.free_rx.try_recv() else {
                            overflowed += 1;
                            continue;
                        };
                        rf.header = header;
                        rf.codec = session.map_or(Codec::Synthetic, |(_, codec)| codec);
                        // The bytes change hands, no copy.
                        if !reassembler.take_completed(header.frame_id, &mut rf.data) {
                            let _ = pipe.free_tx.send(rf);
                            continue;
                        }
                        rf.completed_us = completed_us;
                        rf.offset_us = clock.offset_at(now);
                        if let Err(TrySendError::Full(rf) | TrySendError::Disconnected(rf)) =
                            pipe.decode_tx.try_send(rf)
                        {
                            overflowed += 1;
                            let _ = pipe.free_tx.send(rf);
                        }
                    }
                }
                Packet::Bye(_) => {
                    log::info!("host closed the session");
                    break 'session;
                }
                _ => {}
            }
        }
    }

    // The window may still push; nothing goes out from it any more.
    if let Some(input) = &cfg.input {
        input.connect(None);
    }
    if let Some((session_id, _)) = session {
        let seal_with = crypto.as_deref().map(|t| (t, session_id));
        // Keys and buttons released on the way out (closing releases what
        // the window held) leave before the goodbye.
        if let Some(input) = &cfg.input
            && let Some(n) = input.queue.lock().ok().and_then(|mut q| {
                q.due(Instant::now())
                    .map(|events| InputHeader::encode(session_id, events, &mut out))
            })
        {
            send_packet(socket, seal_with, &out[..n]);
        }
        // More than once: a lost goodbye would keep the session, and
        // anything still pressed, on the host until it times out.
        let n = Bye { session_id }.encode(&mut out);
        for _ in 0..BYE_REPEATS {
            send_packet(socket, seal_with, &out[..n]);
        }
    }
    Ok(NetOutcome {
        receiver: reassembler.totals(),
        overflowed,
        session,
        clock_offset_us: clock.offset_at(now_us()),
        rtt_us: clock.rtt_us(),
    })
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
