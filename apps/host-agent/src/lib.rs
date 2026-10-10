//! Host agent: answers session requests and runs the streaming pipeline.
//!
//! ```text
//! [capture] --slot(1)--> [encode] --fifo(2)--> [packetize + FEC + pacing + send]
//! [control] Hello/HelloAck, clock pings, feedback → FEC + keyframes
//! ```
//!
//! Raw frames meet a latest-frame-wins [`Slot`]: when the encoder is busy,
//! the newest capture replaces an unconsumed one, which costs nothing.
//! Encoded frames reference each other, so they go through a short FIFO and
//! are only dropped when it overflows; that forces a keyframe. Buffers
//! circulate through small free lists, so steady state does not allocate.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use fernsicht_capture::{Frame, FrameSource, TestPattern};
use fernsicht_codec::synthetic::SyntheticEncoder;
#[cfg(feature = "vaapi")]
use fernsicht_codec::vaapi::{VaapiEncoder, VaapiEncoderConfig};
use fernsicht_codec::{EncodedFrame, Encoder};
use fernsicht_core::thread::spawn_hot;
use fernsicht_core::{Slot, clock, now_us};
use fernsicht_net::{FecConfig, FrameMeta, LossEstimator, LossSim, Pacer, Packetizer};
use fernsicht_proto::{Bye, ClockPong, Codec, Feedback, Hello, HelloAck, MAX_DATAGRAM, Packet};

/// Raw frame buffers: producer, slot, consumer.
const BUFFERS: usize = 3;
/// Encoded frames waiting for the sender.
const SEND_QUEUE: usize = 2;
/// Encoded frame buffers: the queue, one being encoded, one being sent.
const ENCODED_BUFFERS: usize = SEND_QUEUE + 2;

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub bind: String,
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u16,
    pub max_bitrate_kbps: u32,
    /// Fraction of video packets to drop on purpose (testing).
    pub loss: f64,
    pub pace_bytes_per_sec: u64,
    /// A session ends when the client has been silent this long.
    pub client_timeout: Duration,
    pub capture: CaptureKind,
    pub encoder: EncoderKind,
}

/// Where session frames come from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CaptureKind {
    /// Moving bar in CPU memory; runs anywhere.
    #[default]
    TestPattern,
    /// The monitor's scanout plane as DMA-BUF (needs the `kms` feature,
    /// CAP_SYS_ADMIN and an encoder that takes DMA-BUFs, i.e. VAAPI; the
    /// synthetic encoder ignores the picture). The encoder scales to the
    /// session resolution.
    Kms {
        /// `/dev/dri/cardN`; `None` picks the first card with a display.
        card: Option<String>,
        /// e.g. `DP-1`; `None` picks the first active display.
        connector: Option<String>,
    },
}

/// Which video encoder sessions use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum EncoderKind {
    /// Realistic frame sizes without real video; runs anywhere.
    #[default]
    Synthetic,
    /// Hardware H.264 over VAAPI (needs the `vaapi` feature and a GPU).
    Vaapi { render_node: String },
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:47800".into(),
            max_width: 1920,
            max_height: 1080,
            max_fps: 144,
            max_bitrate_kbps: 80_000,
            loss: 0.0,
            pace_bytes_per_sec: 50_000_000,
            client_timeout: Duration::from_secs(5),
            capture: CaptureKind::default(),
            encoder: EncoderKind::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionParams {
    pub session_id: u32,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_kbps: u32,
}

/// State the control loop shares with the pipeline threads.
/// Cumulative counters across all sessions of one host agent.
#[derive(Debug, Default)]
pub struct HostStats {
    pub sessions: AtomicU64,
    pub frames_captured: AtomicU64,
    pub frames_encoded: AtomicU64,
    pub keyframes_encoded: AtomicU64,
    /// Encoded frames dropped because the sender was a whole queue behind.
    pub send_overflows: AtomicU64,
    pub frames_sent: AtomicU64,
}

impl HostStats {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

struct Shared {
    stats: Arc<HostStats>,
    running: AtomicBool,
    keyframe_requested: AtomicBool,
    /// Loss rate FEC is sized for, as `f32` bits.
    fec_loss: AtomicU32,
}

struct Session {
    params: SessionParams,
    codec: Codec,
    peer: SocketAddr,
    last_seen: Instant,
    shared: Arc<Shared>,
    loss: LossEstimator,
    threads: Vec<JoinHandle<()>>,
    frame_slot: Arc<Slot<Frame>>,
}

impl Session {
    fn stop(mut self) {
        self.shared.running.store(false, Ordering::Release);
        self.frame_slot.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

pub struct HostAgent {
    cfg: HostConfig,
    socket: Arc<UdpSocket>,
    stats: Arc<HostStats>,
}

impl HostAgent {
    pub fn bind(cfg: HostConfig) -> anyhow::Result<Self> {
        let socket = fernsicht_net::socket::bind_udp(&cfg.bind)
            .with_context(|| format!("bind {}", cfg.bind))?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok(Self {
            cfg,
            socket: Arc::new(socket),
            stats: Arc::default(),
        })
    }

    /// Counters that stay readable while and after [`run`](Self::run) runs.
    pub fn stats(&self) -> Arc<HostStats> {
        self.stats.clone()
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Serves sessions until `stop` is set.
    pub fn run(self, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
        let mut buf = [0u8; 2048];
        let mut out = [0u8; MAX_DATAGRAM];
        let mut session: Option<Session> = None;

        while !stop.load(Ordering::Relaxed) {
            if session
                .as_ref()
                .is_some_and(|s| s.last_seen.elapsed() > self.cfg.client_timeout)
            {
                let s = session.take().unwrap();
                log::info!("session {:08x}: client timed out", s.params.session_id);
                s.stop();
            }
            let (len, from) = match self.socket.recv_from(&mut buf) {
                Ok(r) => r,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    log::warn!("recv: {e}");
                    continue;
                }
            };
            let recv_us = now_us();
            let packet = match Packet::decode(&buf[..len]) {
                Ok(p) => p,
                Err(e) => {
                    log::debug!("{from}: dropping datagram: {e}");
                    continue;
                }
            };
            let from_peer = session.as_ref().is_some_and(|s| s.peer == from);
            if from_peer && let Some(s) = session.as_mut() {
                s.last_seen = Instant::now();
            }

            match packet {
                Packet::Hello(hello) => {
                    if !from_peer {
                        if let Some(old) = session.take() {
                            log::info!("session {:08x}: replaced by {from}", old.params.session_id);
                            old.stop();
                        }
                        let params = self.negotiate(&hello);
                        log::info!(
                            "session {:08x}: {from} {}x{}@{} {} kbit/s",
                            params.session_id,
                            params.width,
                            params.height,
                            params.fps,
                            params.bitrate_kbps
                        );
                        match self.start_session(params, from) {
                            Ok(s) => session = Some(s),
                            Err(e) => {
                                // No ack: the client keeps asking and times out.
                                log::error!("session {:08x}: {e:#}", params.session_id);
                                continue;
                            }
                        }
                    }
                    let Some(s) = session.as_ref() else { continue };
                    let (p, codec) = (s.params, s.codec);
                    let ack = HelloAck {
                        session_id: p.session_id,
                        width: p.width,
                        height: p.height,
                        fps: p.fps,
                        codec,
                    };
                    let n = ack.encode(&mut out);
                    let _ = self.socket.send_to(&out[..n], from);
                }
                Packet::ClockPing(ping) => {
                    let pong = ClockPong {
                        seq: ping.seq,
                        client_send_us: ping.client_send_us,
                        host_recv_us: recv_us,
                        host_send_us: now_us(),
                    };
                    let n = pong.encode(&mut out);
                    let _ = self.socket.send_to(&out[..n], from);
                }
                Packet::Feedback(fb) if from_peer => {
                    if let Some(s) = session.as_mut()
                        && fb.session_id == s.params.session_id
                    {
                        on_feedback(s, &fb);
                    }
                }
                Packet::Bye(bye) if from_peer => {
                    if let Some(s) = session.take() {
                        if bye.session_id == s.params.session_id {
                            log::info!("session {:08x}: closed by client", s.params.session_id);
                            s.stop();
                        } else {
                            session = Some(s);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(s) = session.take() {
            let n = Bye {
                session_id: s.params.session_id,
            }
            .encode(&mut out);
            let _ = self.socket.send_to(&out[..n], s.peer);
            s.stop();
        }
        Ok(())
    }

    fn negotiate(&self, hello: &Hello) -> SessionParams {
        let pick = |want: u16, max: u16| if want == 0 { max } else { want.min(max) };
        SessionParams {
            session_id: new_session_id(),
            width: pick(hello.width, self.cfg.max_width) & !1,
            height: pick(hello.height, self.cfg.max_height) & !1,
            fps: pick(hello.fps, self.cfg.max_fps).max(1),
            bitrate_kbps: if hello.bitrate_kbps == 0 {
                20_000.min(self.cfg.max_bitrate_kbps)
            } else {
                hello.bitrate_kbps.min(self.cfg.max_bitrate_kbps)
            },
        }
    }

    fn start_session(&self, params: SessionParams, peer: SocketAddr) -> anyhow::Result<Session> {
        let loss = LossEstimator::default();
        HostStats::bump(&self.stats.sessions);
        let shared = Arc::new(Shared {
            stats: self.stats.clone(),
            running: AtomicBool::new(true),
            keyframe_requested: AtomicBool::new(true),
            fec_loss: AtomicU32::new(loss.estimate().to_bits()),
        });
        // Create the encoder first: if the GPU is unavailable the session
        // fails before any thread starts.
        let encoder = make_encoder(&self.cfg.encoder, &params)?;
        let codec = encoder.codec();
        let frame_slot = Arc::new(Slot::new());
        let (send_tx, send_rx) = bounded::<EncodedFrame>(SEND_QUEUE);

        let source = make_source(&self.cfg.capture, &params)?;
        let (free_frames_tx, free_frames_rx) = bounded(BUFFERS);
        for _ in 0..BUFFERS {
            free_frames_tx.send(source.alloc_frame()).unwrap();
        }
        let (free_enc_tx, free_enc_rx) = bounded(ENCODED_BUFFERS);
        for _ in 0..ENCODED_BUFFERS {
            free_enc_tx.send(EncodedFrame::default()).unwrap();
        }

        let capture = {
            let (shared, slot) = (shared.clone(), frame_slot.clone());
            let (free_rx, free_tx) = (free_frames_rx, free_frames_tx.clone());
            spawn_hot("capture", move || {
                capture_loop(source, &shared, &slot, &free_rx, &free_tx);
            })?
        };
        let encode = {
            let (shared, in_slot) = (shared.clone(), frame_slot.clone());
            let free_enc_tx = free_enc_tx.clone();
            spawn_hot("encode", move || {
                encode_loop(
                    encoder,
                    &shared,
                    &in_slot,
                    &send_tx,
                    &free_frames_tx,
                    &free_enc_rx,
                    &free_enc_tx,
                );
            })?
        };
        let send = {
            let (shared, socket) = (shared.clone(), self.socket.clone());
            let pacer = Pacer {
                rate_bytes_per_sec: self.cfg.pace_bytes_per_sec,
                burst: 4,
            };
            let loss = LossSim::new(self.cfg.loss, u64::from(params.session_id) | 1);
            spawn_hot("send", move || {
                if let Err(e) = send_loop(
                    &shared,
                    &send_rx,
                    &free_enc_tx,
                    &socket,
                    peer,
                    params,
                    pacer,
                    loss,
                ) {
                    log::error!("send: {e:#}");
                }
            })?
        };

        Ok(Session {
            params,
            codec,
            peer,
            last_seen: Instant::now(),
            shared,
            loss,
            threads: vec![capture, encode, send],
            frame_slot,
        })
    }
}

fn on_feedback(s: &mut Session, fb: &Feedback) {
    let estimate = s.loss.update(fb.loss_ratio());
    s.shared
        .fec_loss
        .store(estimate.to_bits(), Ordering::Relaxed);
    if fb.request_keyframe {
        s.shared.keyframe_requested.store(true, Ordering::Relaxed);
    }
    log::debug!(
        "feedback: loss {:.2} % recovered {} dropped {} → FEC sized for {:.1} %",
        fb.loss_ratio() * 100.0,
        fb.packets_recovered,
        fb.frames_dropped,
        estimate * 100.0
    );
}

fn new_session_id() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.as_nanos() as u32) ^ std::process::id().rotate_left(16) ^ 0x5EED_F00D
}

fn capture_loop(
    mut source: Box<dyn FrameSource>,
    shared: &Shared,
    slot: &Slot<Frame>,
    free_rx: &Receiver<Frame>,
    free_tx: &Sender<Frame>,
) {
    while shared.running.load(Ordering::Acquire) {
        let Ok(mut frame) = free_rx.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        if let Err(e) = source.next_frame(&mut frame) {
            log::error!("capture: {e}");
            break;
        }
        HostStats::bump(&shared.stats.frames_captured);
        if let Some(skipped) = slot.put(frame) {
            let _ = free_tx.send(skipped);
        }
    }
    slot.close();
}

fn make_source(kind: &CaptureKind, p: &SessionParams) -> anyhow::Result<Box<dyn FrameSource>> {
    match kind {
        CaptureKind::TestPattern => Ok(Box::new(TestPattern::new(
            u32::from(p.width),
            u32::from(p.height),
            u32::from(p.fps),
        ))),
        #[cfg(feature = "kms")]
        CaptureKind::Kms { card, connector } => {
            let cap =
                fernsicht_capture::kms::KmsCapture::open(&fernsicht_capture::kms::KmsConfig {
                    card: card.as_ref().map(Into::into),
                    connector: connector.clone(),
                    fps: u32::from(p.fps),
                })?;
            log::info!(
                "capturing {}×{} over KMS ({:?})",
                cap.width(),
                cap.height(),
                cap.selection()
            );
            Ok(Box::new(cap))
        }
        #[cfg(not(feature = "kms"))]
        CaptureKind::Kms { .. } => {
            anyhow::bail!("this build has no KMS capture (cargo feature \"kms\")")
        }
    }
}

fn make_encoder(kind: &EncoderKind, p: &SessionParams) -> anyhow::Result<Box<dyn Encoder>> {
    match kind {
        EncoderKind::Synthetic => Ok(Box::new(SyntheticEncoder::new(
            p.bitrate_kbps,
            u32::from(p.fps),
        ))),
        #[cfg(feature = "vaapi")]
        EncoderKind::Vaapi { render_node } => {
            Ok(Box::new(VaapiEncoder::new(&VaapiEncoderConfig {
                render_node: render_node.clone(),
                width: u32::from(p.width),
                height: u32::from(p.height),
                fps: u32::from(p.fps),
                bitrate_kbps: p.bitrate_kbps,
            })?))
        }
        #[cfg(not(feature = "vaapi"))]
        EncoderKind::Vaapi { .. } => {
            anyhow::bail!("this build has no VAAPI support (cargo feature \"vaapi\")")
        }
    }
}

fn encode_loop(
    mut encoder: Box<dyn Encoder>,
    shared: &Shared,
    in_slot: &Slot<Frame>,
    send_tx: &Sender<EncodedFrame>,
    free_frames: &Sender<Frame>,
    free_enc_rx: &Receiver<EncodedFrame>,
    free_enc_tx: &Sender<EncodedFrame>,
) {
    let mut next_frame_id = 0u32;
    while let Some(frame) = in_slot.take() {
        if !shared.running.load(Ordering::Acquire) {
            break;
        }
        if shared.keyframe_requested.swap(false, Ordering::Relaxed) {
            encoder.request_keyframe();
        }
        let Ok(mut out) = free_enc_rx.recv_timeout(Duration::from_millis(100)) else {
            let _ = free_frames.send(frame);
            continue;
        };
        let result = encoder.encode(&frame, &mut out);
        let _ = free_frames.send(frame);
        match result {
            Ok(()) => {
                HostStats::bump(&shared.stats.frames_encoded);
                if out.keyframe {
                    HostStats::bump(&shared.stats.keyframes_encoded);
                }
                out.frame_id = next_frame_id;
                next_frame_id = next_frame_id.wrapping_add(1);
                match send_tx.try_send(out) {
                    Ok(()) => {}
                    // The sender is a whole queue behind (network or CPU
                    // overload). Later frames reference this one, so resync
                    // with a keyframe; the client sees the frame-id gap too.
                    Err(TrySendError::Full(out)) => {
                        HostStats::bump(&shared.stats.send_overflows);
                        encoder.request_keyframe();
                        let _ = free_enc_tx.send(out);
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            Err(e) => {
                log::warn!("encode: {e}");
                let _ = free_enc_tx.send(out);
            }
        }
    }
    // Dropping `send_tx` on return ends the send loop.
}

#[allow(clippy::too_many_arguments)]
fn send_loop(
    shared: &Shared,
    queue: &Receiver<EncodedFrame>,
    free_enc: &Sender<EncodedFrame>,
    socket: &UdpSocket,
    peer: SocketAddr,
    params: SessionParams,
    pacer: Pacer,
    mut loss: LossSim,
) -> anyhow::Result<()> {
    let mut packetizer = Packetizer::new(FecConfig::default())?;
    let budget = Duration::from_micros(clock::frame_interval_us(u32::from(params.fps)) / 2);
    let mut window_start = Instant::now();
    let (mut frames, mut bytes, mut packets) = (0u32, 0u64, 0u64);

    while let Ok(enc) = queue.recv() {
        if !shared.running.load(Ordering::Acquire) {
            break;
        }
        packetizer.set_loss(f32::from_bits(shared.fec_loss.load(Ordering::Relaxed)));
        let meta = FrameMeta {
            session_id: params.session_id,
            frame_id: enc.frame_id,
            keyframe: enc.keyframe,
            capture_us: enc.capture_us,
            capture_ready_delta_us: enc.capture_ready_us.saturating_sub(enc.capture_us) as u32,
            encoded_delta_us: enc.encoded_us.saturating_sub(enc.capture_us) as u32,
        };
        let pkts = packetizer.packetize(&meta, &enc.data)?;
        let _ = free_enc.send(enc);
        pacer.pace(pkts, budget, |p| {
            packets += 1;
            bytes += p.len() as u64;
            if loss.drop_packet() {
                return Ok(());
            }
            match socket.send_to(p, peer) {
                Ok(_) => Ok(()),
                // A full socket buffer is congestion, not a reason to stop.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
                Err(e) => Err(e),
            }
        })?;
        frames += 1;
        HostStats::bump(&shared.stats.frames_sent);

        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_secs(5) {
            let secs = elapsed.as_secs_f64();
            log::info!(
                "session {:08x}: {:.1} fps, {:.1} Mbit/s, {:.0} pkt/s, FEC sized for {:.1} % loss",
                params.session_id,
                f64::from(frames) / secs,
                bytes as f64 * 8.0 / secs / 1e6,
                packets as f64 / secs,
                packetizer.config().loss * 100.0
            );
            window_start = Instant::now();
            (frames, bytes, packets) = (0, 0, 0);
        }
    }
    Ok(())
}
