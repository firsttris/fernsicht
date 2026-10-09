//! Host agent: answers session requests and runs the streaming pipeline.
//!
//! ```text
//! [capture] --slot(1)--> [encode] --slot(1)--> [packetize + FEC + pacing + send]
//! [control] Hello/HelloAck, clock pings, feedback → FEC + keyframes
//! ```
//!
//! Each arrow is a [`Slot`]: the newest frame replaces an unconsumed one.
//! Buffers circulate through small free lists, so steady state does not
//! allocate.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, bounded};
use fernsicht_capture::{Frame, FrameSource, TestPattern};
use fernsicht_codec::synthetic::SyntheticEncoder;
use fernsicht_codec::{EncodedFrame, Encoder};
use fernsicht_core::thread::spawn_hot;
use fernsicht_core::{Slot, clock, now_us};
use fernsicht_net::{AdaptiveRedundancy, FecConfig, FrameMeta, LossSim, Pacer, Packetizer};
use fernsicht_proto::{Bye, ClockPong, Feedback, Hello, HelloAck, MAX_DATAGRAM, Packet};

/// A session ends when the client has been silent this long.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
/// Buffers per pipeline stage: producer, slot, consumer.
const BUFFERS: usize = 3;

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
struct Shared {
    running: AtomicBool,
    keyframe_requested: AtomicBool,
    /// FEC redundancy as `f32` bits.
    redundancy: AtomicU32,
}

struct Session {
    params: SessionParams,
    peer: SocketAddr,
    last_seen: Instant,
    shared: Arc<Shared>,
    redundancy: AdaptiveRedundancy,
    threads: Vec<JoinHandle<()>>,
    frame_slot: Arc<Slot<Frame>>,
    encoded_slot: Arc<Slot<EncodedFrame>>,
}

impl Session {
    fn stop(mut self) {
        self.shared.running.store(false, Ordering::Release);
        self.frame_slot.close();
        self.encoded_slot.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

pub struct HostAgent {
    cfg: HostConfig,
    socket: Arc<UdpSocket>,
}

impl HostAgent {
    pub fn bind(cfg: HostConfig) -> anyhow::Result<Self> {
        let socket = fernsicht_net::socket::bind_udp(&cfg.bind)
            .with_context(|| format!("bind {}", cfg.bind))?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok(Self {
            cfg,
            socket: Arc::new(socket),
        })
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
            let (len, from) = match self.socket.recv_from(&mut buf) {
                Ok(r) => r,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if let Some(s) = session.take() {
                        if s.last_seen.elapsed() > CLIENT_TIMEOUT {
                            log::info!("session {:08x}: client timed out", s.params.session_id);
                            s.stop();
                        } else {
                            session = Some(s);
                        }
                    }
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
                        session = Some(self.start_session(params, from)?);
                    }
                    let p = session.as_ref().unwrap().params;
                    let ack = HelloAck {
                        session_id: p.session_id,
                        width: p.width,
                        height: p.height,
                        fps: p.fps,
                        codec: fernsicht_proto::Codec::Synthetic,
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
        let redundancy = AdaptiveRedundancy::default();
        let shared = Arc::new(Shared {
            running: AtomicBool::new(true),
            keyframe_requested: AtomicBool::new(true),
            redundancy: AtomicU32::new(redundancy.redundancy().to_bits()),
        });
        let frame_slot = Arc::new(Slot::new());
        let encoded_slot = Arc::new(Slot::new());

        let source = TestPattern::new(
            u32::from(params.width),
            u32::from(params.height),
            u32::from(params.fps),
        );
        let (free_frames_tx, free_frames_rx) = bounded(BUFFERS);
        for _ in 0..BUFFERS {
            free_frames_tx.send(source.alloc_frame()).unwrap();
        }
        let (free_enc_tx, free_enc_rx) = bounded(BUFFERS);
        for _ in 0..BUFFERS {
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
            let (shared, in_slot, out_slot) =
                (shared.clone(), frame_slot.clone(), encoded_slot.clone());
            let free_enc_tx = free_enc_tx.clone();
            let encoder = SyntheticEncoder::new(params.bitrate_kbps, u32::from(params.fps));
            spawn_hot("encode", move || {
                encode_loop(
                    encoder,
                    &shared,
                    &in_slot,
                    &out_slot,
                    &free_frames_tx,
                    &free_enc_rx,
                    &free_enc_tx,
                );
            })?
        };
        let send = {
            let (shared, slot, socket) =
                (shared.clone(), encoded_slot.clone(), self.socket.clone());
            let pacer = Pacer {
                rate_bytes_per_sec: self.cfg.pace_bytes_per_sec,
                burst: 4,
            };
            let loss = LossSim::new(self.cfg.loss, u64::from(params.session_id) | 1);
            spawn_hot("send", move || {
                if let Err(e) = send_loop(
                    &shared,
                    &slot,
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
            peer,
            last_seen: Instant::now(),
            shared,
            redundancy,
            threads: vec![capture, encode, send],
            frame_slot,
            encoded_slot,
        })
    }
}

fn on_feedback(s: &mut Session, fb: &Feedback) {
    let r = s.redundancy.update(fb.loss_ratio());
    s.shared.redundancy.store(r.to_bits(), Ordering::Relaxed);
    if fb.request_keyframe {
        s.shared.keyframe_requested.store(true, Ordering::Relaxed);
    }
    log::debug!(
        "feedback: loss {:.2} % recovered {} dropped {} → FEC {:.0} %",
        fb.loss_ratio() * 100.0,
        fb.packets_recovered,
        fb.frames_dropped,
        r * 100.0
    );
}

fn new_session_id() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.as_nanos() as u32) ^ std::process::id().rotate_left(16) ^ 0x5EED_F00D
}

fn capture_loop(
    mut source: TestPattern,
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
        if let Some(skipped) = slot.put(frame) {
            let _ = free_tx.send(skipped);
        }
    }
    slot.close();
}

fn encode_loop(
    mut encoder: SyntheticEncoder,
    shared: &Shared,
    in_slot: &Slot<Frame>,
    out_slot: &Slot<EncodedFrame>,
    free_frames: &Sender<Frame>,
    free_enc_rx: &Receiver<EncodedFrame>,
    free_enc_tx: &Sender<EncodedFrame>,
) {
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
                if let Some(skipped) = out_slot.put(out) {
                    // The sender fell behind; the skipped frame may have been
                    // a reference for later ones, so resync with a keyframe.
                    if skipped.keyframe {
                        encoder.request_keyframe();
                    }
                    let _ = free_enc_tx.send(skipped);
                }
            }
            Err(e) => {
                log::warn!("encode: {e}");
                let _ = free_enc_tx.send(out);
            }
        }
    }
    out_slot.close();
}

#[allow(clippy::too_many_arguments)]
fn send_loop(
    shared: &Shared,
    slot: &Slot<EncodedFrame>,
    free_enc: &Sender<EncodedFrame>,
    socket: &UdpSocket,
    peer: SocketAddr,
    params: SessionParams,
    pacer: Pacer,
    mut loss: LossSim,
) -> anyhow::Result<()> {
    let mut packetizer = Packetizer::new(FecConfig::default())?;
    let budget = Duration::from_micros(clock::frame_interval_us(u32::from(params.fps)) / 2);
    let mut frame_id = 0u32;
    let mut window_start = Instant::now();
    let (mut frames, mut bytes, mut packets) = (0u32, 0u64, 0u64);

    while let Some(enc) = slot.take() {
        if !shared.running.load(Ordering::Acquire) {
            break;
        }
        packetizer.set_redundancy(f32::from_bits(shared.redundancy.load(Ordering::Relaxed)));
        let meta = FrameMeta {
            session_id: params.session_id,
            frame_id,
            keyframe: enc.keyframe,
            capture_us: enc.capture_us,
            capture_ready_delta_us: enc.capture_ready_us.saturating_sub(enc.capture_us) as u32,
            encoded_delta_us: enc.encoded_us.saturating_sub(enc.capture_us) as u32,
        };
        frame_id = frame_id.wrapping_add(1);
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

        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_secs(5) {
            let secs = elapsed.as_secs_f64();
            log::info!(
                "session {:08x}: {:.1} fps, {:.1} Mbit/s, {:.0} pkt/s, FEC {:.0} %",
                params.session_id,
                f64::from(frames) / secs,
                bytes as f64 * 8.0 / secs / 1e6,
                packets as f64 / secs,
                packetizer.config().redundancy * 100.0
            );
            window_start = Instant::now();
            (frames, bytes, packets) = (0, 0, 0);
        }
    }
    Ok(())
}
