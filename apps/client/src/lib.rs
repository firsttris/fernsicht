//! Client: a network thread (receive, reassemble, clock sync, feedback)
//! and a decode/present thread.
//!
//! ```text
//! [network] --fifo(4)--> [decode] --latest wins--> [present]
//! ```
//!
//! Compressed frames reference earlier ones, so every received frame is
//! decoded in order; only *decoded* frames are skipped when presentation
//! falls behind. Any gap in the frame ids breaks the reference chain: the
//! decoder then waits for a keyframe and the client asks the host for one.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use fernsicht_codec::synthetic::SyntheticDecoder;
use fernsicht_codec::{CodecError, DecodedFrame, Decoder};
use fernsicht_core::latency::{FrameTimings, LatencyStats, Stage, Summary};
use fernsicht_core::now_us;
use fernsicht_core::thread::{HOT_NICE, raise_priority, spawn_hot};
use fernsicht_net::{ClockSync, LossSim, Reassembler, ReceiverStats};
use fernsicht_proto::{Bye, ClockPing, Codec, Feedback, Hello, MAX_DATAGRAM, Packet, VideoHeader};
use fernsicht_render::overlay::{self, StreamInfo};
use fernsicht_render::{HeadlessPresenter, Presenter};

const HELLO_INTERVAL: Duration = Duration::from_millis(250);
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(100);
const OVERLAY_INTERVAL: Duration = Duration::from_secs(1);
/// Warn when the host acked but no video arrived for this long.
const NO_VIDEO_WARNING: Duration = Duration::from_secs(2);
/// Completed frames waiting for the decoder.
const DECODE_QUEUE: usize = 4;
/// Frame buffers: the queue, one being decoded, one being filled.
const BUFFERS: usize = DECODE_QUEUE + 2;

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
    /// Give up when the host has been silent this long.
    pub host_timeout: Duration,
    /// GPU used for hardware decoding (VAAPI).
    pub render_node: String,
    /// Which hardware H.264 decoder to use.
    pub decoder: DecoderChoice,
    /// Writes the received bitstream here (H.264 Annex B: plays with
    /// `ffplay` or `mpv`). Only frames the decoder gets are written.
    pub record: Option<std::path::PathBuf>,
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
            host_timeout: Duration::from_secs(5),
            render_node: "/dev/dri/renderD128".into(),
            decoder: DecoderChoice::Auto,
            record: None,
        }
    }
}

/// Hardware decoder for H.264 streams.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecoderChoice {
    /// VAAPI if it works (AMD, Intel), else NVDEC (NVIDIA).
    #[default]
    Auto,
    Vaapi,
    /// NVIDIA, CUDA device 0.
    Nvdec,
}

/// Result of a client run.
#[derive(Clone, Debug, Default)]
pub struct RunSummary {
    /// Session id from the host's `HelloAck`, if one arrived.
    pub session_id: Option<u32>,
    pub codec: Option<Codec>,
    pub receiver: ReceiverStats,
    pub frames_presented: u64,
    pub keyframes_decoded: u64,
    /// Decoded but not shown because a newer frame was already waiting.
    pub frames_skipped: u64,
    /// Dropped before decoding because the decoder fell behind.
    pub frames_overflowed: u64,
    /// Frames discarded because the reference chain was broken (no keyframe
    /// yet, or a frame before them was lost).
    pub frames_awaiting_keyframe: u64,
    /// Corrupt frames rejected by the decoder.
    pub decode_errors: u64,
    /// Estimated host clock − client clock at the end of the run.
    pub clock_offset_us: i64,
    pub rtt_us: Option<u64>,
    pub total: Summary,
    pub stages: Vec<(Stage, Summary)>,
}

impl RunSummary {
    pub fn stage(&self, stage: Stage) -> Summary {
        self.stages
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, sum)| *sum)
            .unwrap_or_default()
    }
}

/// What the network thread learned during the run.
struct NetOutcome {
    receiver: ReceiverStats,
    overflowed: u64,
    session: Option<(u32, Codec)>,
    clock_offset_us: i64,
    rtt_us: Option<u64>,
}

/// Tracks whether the decoder can use the next frame. Delta frames need
/// every frame since the last keyframe; after any gap only a keyframe helps.
#[derive(Clone, Copy, Debug, Default)]
pub struct RefChain {
    next: Option<u32>,
}

impl RefChain {
    /// Returns `true` if frame `frame_id` can be decoded now.
    pub fn accept(&mut self, frame_id: u32, keyframe: bool) -> bool {
        if keyframe || self.next == Some(frame_id) {
            self.next = Some(frame_id.wrapping_add(1));
            true
        } else {
            self.next = None;
            false
        }
    }

    /// The decoder rejected a frame: wait for the next keyframe.
    pub fn break_chain(&mut self) {
        self.next = None;
    }

    pub fn needs_keyframe(&self) -> bool {
        self.next.is_none()
    }
}

/// A reassembled frame on its way to the decoder.
struct ReceivedFrame {
    header: VideoHeader,
    /// Codec announced by the host for this session.
    codec: Codec,
    data: Vec<u8>,
    completed_us: u64,
    /// Host clock − client clock at the time the frame completed.
    offset_us: i64,
}

impl ReceivedFrame {
    fn empty() -> Self {
        Self {
            header: VideoHeader::default(),
            codec: Codec::Synthetic,
            data: Vec::new(),
            completed_us: 0,
            offset_us: 0,
        }
    }
}

/// Creates the presenter on the present thread (Vulkan objects stay on the
/// thread that uses them).
pub type PresenterFactory = Box<dyn FnOnce() -> Result<Box<dyn Presenter>, String> + Send>;

/// Connects to `cfg.host` and streams until `stop`, the configured duration,
/// or the host going away. Frames are decoded but not shown.
pub fn run(cfg: ClientConfig, stop: Arc<AtomicBool>) -> anyhow::Result<RunSummary> {
    run_with(
        cfg,
        stop,
        Box::new(|| Ok(Box::new(HeadlessPresenter::default()) as Box<dyn Presenter>)),
    )
}

/// Like [`run`], showing frames with the presenter `make_presenter`
/// creates. If that fails, the client runs headless and says why.
pub fn run_with(
    cfg: ClientConfig,
    stop: Arc<AtomicBool>,
    make_presenter: PresenterFactory,
) -> anyhow::Result<RunSummary> {
    let socket = fernsicht_net::socket::bind_udp("0.0.0.0:0").context("bind")?;
    socket
        .connect(&cfg.host)
        .with_context(|| format!("connect {}", cfg.host))?;
    socket.set_read_timeout(Some(Duration::from_millis(2)))?;

    let (decode_tx, decode_rx) = bounded::<ReceivedFrame>(DECODE_QUEUE);
    let need_keyframe = Arc::new(AtomicBool::new(true));
    let info = Arc::new(Mutex::new(StreamInfo::default()));
    let (free_tx, free_rx) = bounded(BUFFERS);
    for _ in 0..BUFFERS {
        free_tx.send(ReceivedFrame::empty()).unwrap();
    }

    let mut record = match &cfg.record {
        Some(path) => Some(std::io::BufWriter::new(
            std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?,
        )),
        None => None,
    };
    let presenter = {
        let (info, free_tx, need_keyframe) = (info.clone(), free_tx.clone(), need_keyframe.clone());
        let print = cfg.print_overlay;
        let hw = Hw {
            render_node: cfg.render_node.clone(),
            decoder: cfg.decoder,
        };
        spawn_hot("present", move || {
            let mut presenter = make_presenter().unwrap_or_else(|e| {
                log::error!("no video window ({e}); running headless");
                Box::new(HeadlessPresenter::default())
            });
            let r = present_loop(
                &decode_rx,
                &info,
                &free_tx,
                &need_keyframe,
                print,
                &hw,
                record.as_mut().map(|w| w as &mut dyn std::io::Write),
                presenter.as_mut(),
            );
            if let Some(Err(e)) = record.as_mut().map(std::io::Write::flush) {
                log::error!("recording: {e}");
            }
            r
        })?
    };

    if let Err(e) = raise_priority(HOT_NICE) {
        log::debug!("network thread: could not raise priority: {e}");
    }
    let pipe = Pipe {
        decode_tx: &decode_tx,
        free_rx: &free_rx,
        free_tx: &free_tx,
        need_keyframe: &need_keyframe,
    };
    let net = network_loop(&cfg, &socket, &stop, &pipe, &info);
    // Closing the queue ends the present thread once it has drained.
    drop(decode_tx);
    let present = presenter.join().expect("present thread panicked");
    let net = net?;

    Ok(RunSummary {
        session_id: net.session.map(|(id, _)| id),
        codec: net.session.map(|(_, codec)| codec),
        receiver: net.receiver,
        frames_presented: present.presented,
        keyframes_decoded: present.keyframes,
        frames_skipped: present.skipped,
        frames_overflowed: net.overflowed,
        frames_awaiting_keyframe: present.awaiting_keyframe,
        decode_errors: present.decode_errors,
        clock_offset_us: net.clock_offset_us,
        rtt_us: net.rtt_us,
        total: present.stats.total(),
        stages: Stage::ALL
            .iter()
            .map(|s| (*s, present.stats.stage(*s)))
            .collect(),
    })
}

/// The network thread's ends of the decode queue.
struct Pipe<'a> {
    decode_tx: &'a Sender<ReceivedFrame>,
    free_rx: &'a Receiver<ReceivedFrame>,
    free_tx: &'a Sender<ReceivedFrame>,
    /// Set by the decoder while its reference chain is broken.
    need_keyframe: &'a AtomicBool,
}

fn network_loop(
    cfg: &ClientConfig,
    socket: &UdpSocket,
    stop: &AtomicBool,
    pipe: &Pipe<'_>,
    info: &Mutex<StreamInfo>,
) -> anyhow::Result<NetOutcome> {
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
    let mut overflowed = 0u64;
    let mut acked_at: Option<Instant> = None;
    let mut video_seen = false;

    loop {
        if stop.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() >= d) {
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
                request_keyframe: reassembler.needs_keyframe()
                    || pipe.need_keyframe.load(Ordering::Relaxed),
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
                session = Some((ack.session_id, ack.codec));
            }
            Packet::ClockPong(pong) => {
                clock.on_pong(&pong, now);
            }
            Packet::Video(h, payload) => {
                if session.is_none_or(|(id, _)| id != h.session_id) {
                    continue;
                }
                video_seen = true;
                if loss.drop_packet() {
                    continue;
                }
                rates.bytes += len as u64;
                if let Some(done) = reassembler.push(&h, payload, now) {
                    rates.frames += 1;
                    // The decoder fell behind by a whole queue: drop the
                    // frame; the gap makes it ask for a keyframe.
                    let Ok(mut rf) = pipe.free_rx.try_recv() else {
                        overflowed += 1;
                        continue;
                    };
                    rf.header = done.header;
                    rf.codec = session.map_or(Codec::Synthetic, |(_, codec)| codec);
                    rf.data.clear();
                    rf.data.extend_from_slice(done.data);
                    rf.completed_us = done.completed_us;
                    rf.offset_us = clock.offset_us();
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
                break;
            }
            _ => {}
        }
    }

    if let Some((session_id, _)) = session {
        let n = Bye { session_id }.encode(&mut out);
        let _ = socket.send(&out[..n]);
    }
    Ok(NetOutcome {
        receiver: reassembler.totals(),
        overflowed,
        session,
        clock_offset_us: clock.offset_us(),
        rtt_us: clock.rtt_us(),
    })
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

/// What the present thread needs to create a hardware decoder.
struct Hw {
    #[cfg_attr(not(feature = "vaapi"), allow(dead_code))]
    render_node: String,
    decoder: DecoderChoice,
}

fn make_decoder(codec: Codec, hw: &Hw) -> Result<Box<dyn Decoder>, CodecError> {
    match codec {
        Codec::Synthetic => Ok(Box::new(SyntheticDecoder::default())),
        Codec::H264 => make_h264_decoder(hw),
        other => Err(CodecError::Backend(format!(
            "cannot decode {}",
            overlay::codec_label(other)
        ))),
    }
}

fn make_h264_decoder(hw: &Hw) -> Result<Box<dyn Decoder>, CodecError> {
    let mut errors = Vec::new();
    if matches!(hw.decoder, DecoderChoice::Auto | DecoderChoice::Vaapi) {
        #[cfg(feature = "vaapi")]
        match fernsicht_codec::vaapi::VaapiDecoder::new(&hw.render_node) {
            Ok(d) => {
                log::info!("decoding with VAAPI ({})", hw.render_node);
                return Ok(Box::new(d));
            }
            Err(e) => errors.push(format!("VAAPI: {e}")),
        }
        #[cfg(not(feature = "vaapi"))]
        errors.push("VAAPI: not in this build (cargo feature \"vaapi\")".to_string());
    }
    if matches!(hw.decoder, DecoderChoice::Auto | DecoderChoice::Nvdec) {
        #[cfg(feature = "nvidia")]
        match fernsicht_codec::nvidia::NvdecDecoder::new(0) {
            Ok(d) => {
                log::info!("decoding with NVDEC");
                return Ok(Box::new(d));
            }
            Err(e) => errors.push(format!("NVDEC: {e}")),
        }
        #[cfg(not(feature = "nvidia"))]
        errors.push("NVDEC: not in this build (cargo feature \"nvidia\")".to_string());
    }
    Err(CodecError::Backend(format!(
        "no H.264 decoder: {}",
        errors.join("; ")
    )))
}

struct PresentResult {
    stats: LatencyStats,
    presented: u64,
    skipped: u64,
    keyframes: u64,
    awaiting_keyframe: u64,
    decode_errors: u64,
}

#[allow(clippy::too_many_arguments)]
fn present_loop(
    queue: &Receiver<ReceivedFrame>,
    info: &Mutex<StreamInfo>,
    free_tx: &Sender<ReceivedFrame>,
    need_keyframe: &AtomicBool,
    print_overlay: bool,
    hw: &Hw,
    mut record: Option<&mut dyn std::io::Write>,
    presenter: &mut dyn Presenter,
) -> PresentResult {
    // Created on the first frame, from the codec the host announced.
    let mut decoder: Option<(Codec, Box<dyn Decoder>)> = None;
    let mut decoded = DecodedFrame::default();
    let mut chain = RefChain::default();
    let mut r = PresentResult {
        stats: LatencyStats::default(),
        presented: 0,
        skipped: 0,
        keyframes: 0,
        awaiting_keyframe: 0,
        decode_errors: 0,
    };
    let mut last_overlay = Instant::now();

    while let Ok(rf) = queue.recv() {
        let h = rf.header;
        if decoder.as_ref().is_none_or(|(codec, _)| *codec != rf.codec) {
            decoder = match make_decoder(rf.codec, hw) {
                Ok(d) => Some((rf.codec, d)),
                Err(e) => {
                    log::error!("no decoder for {}: {e}", overlay::codec_label(rf.codec));
                    None
                }
            };
            chain = RefChain::default();
        }
        let Some((_, decoder)) = decoder.as_mut() else {
            r.decode_errors += 1;
            let _ = free_tx.send(rf);
            continue;
        };
        if !chain.accept(h.frame_id, h.keyframe) {
            r.awaiting_keyframe += 1;
        } else {
            if let Some(w) = record.as_mut()
                && let Err(e) = w.write_all(&rf.data)
            {
                log::error!("recording stopped: {e}");
                record = None;
            }
            let result = decoder.decode(&rf.data, &mut decoded);
            let decoded_us = now_us();
            match result {
                Ok(()) => {
                    r.keyframes += u64::from(decoded.keyframe);
                    // Latest frame wins at presentation: if a newer frame is
                    // already queued, showing this one would only add latency.
                    if !queue.is_empty() {
                        r.skipped += 1;
                    } else {
                        let picture = match presenter.wants() {
                            Some(kind) => decoder.picture(kind).unwrap_or_else(|e| {
                                log::debug!("picture: {e}");
                                None
                            }),
                            None => None,
                        };
                        match presenter.present(&decoded, picture.as_ref()) {
                            Ok(()) => r.presented += 1,
                            Err(e) => log::warn!("present: {e}"),
                        }
                        let presented_us = now_us();
                        let captured = h.capture_us as i64 - rf.offset_us;
                        r.stats.record(&FrameTimings {
                            captured,
                            capture_ready: captured + i64::from(h.capture_ready_delta_us),
                            encoded: captured + i64::from(h.encoded_delta_us),
                            received: rf.completed_us as i64,
                            decoded: decoded_us as i64,
                            presented: presented_us as i64,
                        });
                    }
                }
                Err(e) => {
                    chain.break_chain();
                    if matches!(e, CodecError::NeedKeyframe) {
                        r.awaiting_keyframe += 1;
                    } else {
                        r.decode_errors += 1;
                        log::debug!("decode: {e}");
                    }
                }
            }
        }
        need_keyframe.store(chain.needs_keyframe(), Ordering::Relaxed);
        let _ = free_tx.send(rf);

        if last_overlay.elapsed() >= OVERLAY_INTERVAL {
            let info = *info.lock().unwrap();
            let lines = overlay::lines(&r.stats, &info);
            presenter.overlay(&lines);
            if print_overlay {
                for line in &lines {
                    println!("{line}");
                }
                println!();
            }
            last_overlay = Instant::now();
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_chain_needs_a_keyframe_first() {
        let mut c = RefChain::default();
        assert!(c.needs_keyframe());
        assert!(!c.accept(0, false));
        assert!(c.accept(1, true));
        assert!(!c.needs_keyframe());
        assert!(c.accept(2, false));
        assert!(c.accept(3, false));
    }

    #[test]
    fn ref_chain_breaks_on_gaps_until_keyframe() {
        let mut c = RefChain::default();
        assert!(c.accept(0, true));
        assert!(!c.accept(2, false), "frame 1 missing");
        assert!(!c.accept(3, false), "still broken");
        assert!(c.needs_keyframe());
        assert!(c.accept(4, true));
        assert!(c.accept(5, false));
    }

    #[test]
    fn ref_chain_breaks_on_decode_error_and_wraps() {
        let mut c = RefChain::default();
        assert!(c.accept(u32::MAX, true));
        assert!(c.accept(0, false));
        c.break_chain();
        assert!(!c.accept(1, false));
        assert!(c.accept(9, true));
    }

    #[test]
    fn rate_window_publishes_ratios() {
        let mut w = RateWindow::new();
        w.bytes = 1_000_000;
        w.frames = 60;
        w.add(&ReceiverStats {
            packets_received: 990,
            packets_lost: 10,
            frames_completed: 59,
            frames_dropped: 1,
            ..ReceiverStats::default()
        });
        let mut info = StreamInfo::default();
        w.publish(&mut info, Codec::H264, Some(500));
        assert_eq!(info.codec, Codec::H264);
        assert!((info.loss_before_fec - 0.01).abs() < 1e-6);
        assert!((info.loss_after_fec - 1.0 / 60.0).abs() < 1e-6);
        assert_eq!(info.rtt_us, Some(500));
        assert!(info.fps > 0.0 && info.bitrate_bps > 0);
        assert_eq!(ratio(1, 0), 0.0);
    }
}
