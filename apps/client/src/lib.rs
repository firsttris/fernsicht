//! Client: network thread (receive, reassemble, clock sync, feedback) and
//! a decode/present thread, connected by a latest-frame-wins [`Slot`].

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, bounded};
use fernsicht_codec::synthetic::SyntheticDecoder;
use fernsicht_codec::{DecodedFrame, Decoder};
use fernsicht_core::latency::{FrameTimings, LatencyStats, Summary};
use fernsicht_core::thread::{HOT_NICE, raise_priority, spawn_hot};
use fernsicht_core::{Slot, now_us};
use fernsicht_net::{ClockSync, LossSim, Reassembler, ReceiverStats};
use fernsicht_proto::{Bye, ClockPing, Codec, Feedback, Hello, MAX_DATAGRAM, Packet, VideoHeader};
use fernsicht_render::overlay::{self, StreamInfo};
use fernsicht_render::{HeadlessPresenter, Presenter};

const HELLO_INTERVAL: Duration = Duration::from_millis(250);
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(100);
const OVERLAY_INTERVAL: Duration = Duration::from_secs(1);
const HOST_TIMEOUT: Duration = Duration::from_secs(5);
const BUFFERS: usize = 3;

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub host: String,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_kbps: u32,
    /// Fraction of incoming video packets to drop on purpose (testing).
    pub loss: f64,
    pub duration: Option<Duration>,
    pub print_overlay: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1:47800".into(),
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            loss: 0.0,
            duration: None,
            print_overlay: false,
        }
    }
}

/// Result of a client run.
#[derive(Clone, Debug, Default)]
pub struct RunSummary {
    pub receiver: ReceiverStats,
    pub frames_presented: u64,
    pub decode_errors: u64,
    pub total: Summary,
    pub stages: Vec<(&'static str, Summary)>,
}

/// A reassembled frame on its way to the decoder.
struct ReceivedFrame {
    header: VideoHeader,
    data: Vec<u8>,
    completed_us: u64,
    /// Host clock − client clock at the time the frame completed.
    offset_us: i64,
}

impl ReceivedFrame {
    fn empty() -> Self {
        Self {
            header: VideoHeader::default(),
            data: Vec::new(),
            completed_us: 0,
            offset_us: 0,
        }
    }
}

/// Connects to `cfg.host` and streams until `stop`, the configured duration,
/// or the host going away.
pub fn run(cfg: ClientConfig, stop: Arc<AtomicBool>) -> anyhow::Result<RunSummary> {
    let socket = fernsicht_net::socket::bind_udp("0.0.0.0:0").context("bind")?;
    socket
        .connect(&cfg.host)
        .with_context(|| format!("connect {}", cfg.host))?;
    socket.set_read_timeout(Some(Duration::from_millis(2)))?;

    let slot = Arc::new(Slot::<ReceivedFrame>::new());
    let info = Arc::new(Mutex::new(StreamInfo::default()));
    let (free_tx, free_rx) = bounded(BUFFERS);
    for _ in 0..BUFFERS {
        free_tx.send(ReceivedFrame::empty()).unwrap();
    }

    let presenter = {
        let (slot, info, free_tx) = (slot.clone(), info.clone(), free_tx.clone());
        let print = cfg.print_overlay;
        spawn_hot("present", move || {
            present_loop(&slot, &info, &free_tx, print)
        })?
    };

    if let Err(e) = raise_priority(HOT_NICE) {
        log::debug!("network thread: could not raise priority: {e}");
    }
    let net = network_loop(&cfg, &socket, &stop, &slot, &info, &free_rx, &free_tx);
    slot.close();
    let present = presenter.join().expect("present thread panicked");
    let receiver = net?;

    Ok(RunSummary {
        receiver,
        frames_presented: present.presented,
        decode_errors: present.decode_errors,
        total: present.stats.total(),
        stages: fernsicht_core::latency::Stage::ALL
            .iter()
            .map(|s| (s.label(), present.stats.stage(*s)))
            .collect(),
    })
}

fn network_loop(
    cfg: &ClientConfig,
    socket: &UdpSocket,
    stop: &AtomicBool,
    slot: &Slot<ReceivedFrame>,
    info: &Mutex<StreamInfo>,
    free_rx: &Receiver<ReceivedFrame>,
    free_tx: &Sender<ReceivedFrame>,
) -> anyhow::Result<ReceiverStats> {
    let started = Instant::now();
    let deadline = cfg.duration.map(|d| started + d);
    let hello = Hello {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        bitrate_kbps: cfg.bitrate_kbps,
    };
    let mut buf = [0u8; 2048];
    let mut out = [0u8; MAX_DATAGRAM];
    let mut reassembler = Reassembler::new();
    let mut clock = ClockSync::new();
    let mut loss = LossSim::new(cfg.loss, 0xF00D);
    let mut session: Option<(u32, Codec)> = None;
    let mut last_hello: Option<Instant> = None;
    let mut last_ping: Option<Instant> = None;
    let mut ping_seq = 0u32;
    let mut last_feedback = Instant::now();
    let mut last_packet = Instant::now();
    let mut rates = RateWindow::new();

    loop {
        if stop.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        if last_packet.elapsed() > HOST_TIMEOUT {
            anyhow::bail!("no packets from {} for {:?}", cfg.host, HOST_TIMEOUT);
        }
        if session.is_none() && last_hello.is_none_or(|t| t.elapsed() >= HELLO_INTERVAL) {
            let n = hello.encode(&mut out);
            let _ = socket.send(&out[..n]);
            last_hello = Some(Instant::now());
        }
        // Ping fast until the clock offset settles, then slowly to track drift.
        let ping_every = if ping_seq < 20 {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(500)
        };
        if last_ping.is_none_or(|t| t.elapsed() >= ping_every) {
            let ping = ClockPing {
                seq: ping_seq,
                client_send_us: now_us(),
            };
            let n = ping.encode(&mut out);
            let _ = socket.send(&out[..n]);
            ping_seq += 1;
            last_ping = Some(Instant::now());
        }
        if let Some((session_id, codec)) = session
            && last_feedback.elapsed() >= FEEDBACK_INTERVAL
        {
            let s = reassembler.take_interval();
            let fb = Feedback {
                session_id,
                request_keyframe: reassembler.needs_keyframe(),
                highest_frame_id: s.highest_frame_id,
                frames_completed: s.frames_completed,
                frames_dropped: s.frames_dropped,
                packets_received: s.packets_received,
                packets_lost: s.packets_lost,
                packets_recovered: s.packets_recovered,
            };
            let n = fb.encode(&mut out);
            let _ = socket.send(&out[..n]);
            rates.add(&s);
            if rates.started.elapsed() >= OVERLAY_INTERVAL
                && let Ok(mut i) = info.lock()
            {
                rates.publish(&mut i, codec, clock.rtt_us());
                rates = RateWindow::new();
            }
            last_feedback = Instant::now();
        }

        let len = match socket.recv(&mut buf) {
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
        last_packet = Instant::now();
        let Ok(packet) = Packet::decode(&buf[..len]) else {
            continue;
        };
        match packet {
            Packet::HelloAck(ack) => {
                if session.is_none() {
                    log::info!(
                        "session {:08x}: {}x{}@{} {}",
                        ack.session_id,
                        ack.width,
                        ack.height,
                        ack.fps,
                        overlay::codec_label(ack.codec)
                    );
                }
                session = Some((ack.session_id, ack.codec));
            }
            Packet::ClockPong(pong) => {
                clock.on_pong(&pong, now);
            }
            Packet::Video(h, payload) => {
                if session.is_none_or(|(id, _)| id != h.session_id) {
                    continue;
                }
                if loss.drop_packet() {
                    continue;
                }
                rates.bytes += len as u64;
                if let Some(done) = reassembler.push(&h, payload, now) {
                    rates.frames += 1;
                    let Ok(mut rf) = free_rx.try_recv() else {
                        log::warn!("no free frame buffer; dropping frame");
                        continue;
                    };
                    rf.header = done.header;
                    rf.data.clear();
                    rf.data.extend_from_slice(done.data);
                    rf.completed_us = done.completed_us;
                    rf.offset_us = clock.offset_us();
                    if let Some(skipped) = slot.put(rf) {
                        let _ = free_tx.send(skipped);
                    }
                }
            }
            Packet::Bye(_) => {
                log::info!("host closed the session");
                break;
            }
            _ => {}
        }
    }

    if let Some((session_id, _)) = session {
        let n = Bye { session_id }.encode(&mut out);
        let _ = socket.send(&out[..n]);
    }
    Ok(reassembler.totals())
}

/// Accumulates receive rates for the overlay over [`OVERLAY_INTERVAL`], so
/// the numbers don't jump with every 100 ms feedback interval.
struct RateWindow {
    started: Instant,
    bytes: u64,
    frames: u32,
    stats: ReceiverStats,
}

impl RateWindow {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            bytes: 0,
            frames: 0,
            stats: ReceiverStats::default(),
        }
    }

    fn add(&mut self, s: &ReceiverStats) {
        self.stats.packets_received += s.packets_received;
        self.stats.packets_lost += s.packets_lost;
        self.stats.frames_completed += s.frames_completed;
        self.stats.frames_dropped += s.frames_dropped;
    }

    fn publish(&self, info: &mut StreamInfo, codec: Codec, rtt_us: Option<u64>) {
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

fn ratio(part: u32, total: u32) -> f32 {
    if total == 0 {
        0.0
    } else {
        part as f32 / total as f32
    }
}

struct PresentResult {
    stats: LatencyStats,
    presented: u64,
    decode_errors: u64,
}

fn present_loop(
    slot: &Slot<ReceivedFrame>,
    info: &Mutex<StreamInfo>,
    free_tx: &Sender<ReceivedFrame>,
    print_overlay: bool,
) -> PresentResult {
    let mut decoder = SyntheticDecoder::default();
    let mut presenter = HeadlessPresenter::default();
    let mut decoded = DecodedFrame::default();
    let mut stats = LatencyStats::default();
    let mut decode_errors = 0u64;
    let mut last_overlay = Instant::now();

    while let Some(rf) = slot.take() {
        let result = decoder.decode(&rf.data, &mut decoded);
        let decoded_us = now_us();
        match result {
            Ok(()) => {
                if let Err(e) = presenter.present(&decoded) {
                    log::warn!("present: {e}");
                }
                let presented_us = now_us();
                let h = &rf.header;
                let captured = h.capture_us as i64 - rf.offset_us;
                stats.record(&FrameTimings {
                    captured,
                    capture_ready: captured + i64::from(h.capture_ready_delta_us),
                    encoded: captured + i64::from(h.encoded_delta_us),
                    received: rf.completed_us as i64,
                    decoded: decoded_us as i64,
                    presented: presented_us as i64,
                });
            }
            Err(e) => {
                decode_errors += 1;
                log::debug!("decode: {e}");
            }
        }
        let _ = free_tx.send(rf);

        if print_overlay && last_overlay.elapsed() >= OVERLAY_INTERVAL {
            let info = *info.lock().unwrap();
            for line in overlay::lines(&stats, &info) {
                println!("{line}");
            }
            println!();
            last_overlay = Instant::now();
        }
    }
    PresentResult {
        stats,
        presented: presenter.presented,
        decode_errors,
    }
}
