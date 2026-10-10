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

pub mod control;
mod layout;

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use fernsicht_audio::{AudioSource, Frame as AudioFrame, Tone};
use fernsicht_capture::{CursorState, Frame, FrameSource, TestPattern};
use fernsicht_codec::synthetic::SyntheticEncoder;
#[cfg(feature = "vaapi")]
use fernsicht_codec::vaapi::{VaapiEncoder, VaapiEncoderConfig};
use fernsicht_codec::{EncodedFrame, Encoder};
use fernsicht_core::thread::spawn_hot;
use fernsicht_core::{Slot, clock, now_us};
use fernsicht_input::uinput::Uinput;
use fernsicht_input::{Dedup, InputSink, Recorder};
use fernsicht_net::{FecConfig, FrameMeta, LossEstimator, LossSim, Pacer, Packetizer};
use fernsicht_proto::{
    AudioHeader, Bye, CURSOR_CHUNK, ClockPong, Codec, Cursor, CursorShape, Feedback, Handshake,
    Hello, HelloAck, InputAck, InputHeader, MAX_AUDIO_FRAME, MAX_CURSOR_SIZE, MAX_DATAGRAM, Packet,
    Pair, RejectReason, SealedHeader,
};
use fernsicht_secure::pairing::HostPairing;
use fernsicht_secure::session::{Responder, Transport};
use fernsicht_secure::{Identity, Peer, PublicKey, Trusted};

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
    /// What happens to the client's mouse and keyboard input.
    pub input: InputKind,
    /// Sound sent along.
    pub audio: AudioKind,
    /// Pairing and encryption. `None` (the library default) accepts anyone
    /// unencrypted: for tests only; the program always sets it.
    pub security: Option<Arc<HostSecurity>>,
    /// Local control socket (pairing, status) while running; needs
    /// `security`. See [`control`].
    pub control: Option<PathBuf>,
}

/// Where the sound comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioKind {
    /// No sound (the library default, so tests stay quiet).
    #[default]
    Off,
    /// What this computer plays (PipeWire/PulseAudio monitor).
    Desktop,
    /// A 440 Hz test tone.
    Tone,
}

/// Where the client's input goes.
#[derive(Clone, Debug, Default)]
pub enum InputKind {
    /// Ignored (acknowledged, so the client stops resending). The default:
    /// there is no authentication yet, anyone who reaches the port could
    /// type on this machine.
    #[default]
    Off,
    /// Virtual keyboard and mice through /dev/uinput, created per session.
    Uinput,
    /// Recorded (tests).
    Record(Recorder),
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
    /// Hardware H.264 with NVENC (needs the `nvidia` feature and an NVIDIA
    /// GPU; `gpu` is the CUDA device index). Test pattern only for now.
    Nvenc { gpu: u32 },
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:47800".into(),
            max_width: 3840,
            max_height: 2160,
            max_fps: 144,
            max_bitrate_kbps: 80_000,
            loss: 0.0,
            pace_bytes_per_sec: 50_000_000,
            client_timeout: Duration::from_secs(5),
            capture: CaptureKind::default(),
            encoder: EncoderKind::default(),
            input: InputKind::default(),
            audio: AudioKind::default(),
            security: None,
            control: None,
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
    pub audio_frames_sent: AtomicU64,
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
    /// The client lacks the pointer image.
    cursor_requested: AtomicBool,
    /// Loss rate FEC is sized for, as `f32` bits.
    fec_loss: AtomicU32,
}

struct Session {
    params: SessionParams,
    codec: Codec,
    peer: SocketAddr,
    /// Sends to the client (sealed for secure sessions).
    link: Link,
    /// The handshake that started it (first message and our answer), to
    /// answer a retransmission the same.
    handshake: Option<(Vec<u8>, Vec<u8>)>,
    last_seen: Instant,
    shared: Arc<Shared>,
    loss: LossEstimator,
    threads: Vec<JoinHandle<()>>,
    frame_slot: Arc<Slot<Frame>>,
    /// Released (all keys up) when the session ends.
    input: Option<Box<dyn InputSink>>,
    input_seen: Dedup,
    input_errors: RepeatedError,
}

impl Session {
    /// Applies the input events not applied yet, in order.
    fn apply_input(&mut self, body: &[u8]) {
        for (seq, event) in InputHeader::events(body) {
            if !self.input_seen.accept(seq) {
                continue;
            }
            if let Some(sink) = self.input.as_mut()
                && let Err(e) = sink.inject(&event)
                && let Some(msg) = self.input_errors.report(e, Instant::now())
            {
                log::warn!("input: {msg}");
            }
        }
    }

    fn stop(mut self) {
        if let Some(mut input) = self.input.take() {
            input.release_all();
        }
        self.shared.running.store(false, Ordering::Release);
        self.frame_slot.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Sends a session's packets to its client, sealed when the session is
/// secure. Shared by the session's threads.
#[derive(Clone)]
struct Link {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    session_id: u32,
    crypto: Option<Arc<Transport>>,
}

impl Link {
    fn send(&self, packet: &[u8]) -> std::io::Result<()> {
        let Some(crypto) = &self.crypto else {
            return self.socket.send_to(packet, self.peer).map(|_| ());
        };
        let mut buf = [0u8; MAX_DATAGRAM];
        let (counter, n) = crypto
            .seal(packet, &mut buf[SealedHeader::LEN..])
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        SealedHeader {
            session_id: self.session_id,
            counter,
        }
        .write(&mut buf);
        self.socket
            .send_to(&buf[..SealedHeader::LEN + n], self.peer)
            .map(|_| ())
    }
}

/// How long pairing mode stays open, and how many wrong PINs it takes.
pub const PAIRING_OPEN_FOR: Duration = Duration::from_secs(300);
pub const PAIRING_ATTEMPTS: u8 = 3;

struct PairingWindow {
    pin: String,
    until: Instant,
    attempts_left: u8,
}

/// Who this host is and whom it lets in. Without it (library default, for
/// tests) sessions are not encrypted and anyone may connect.
pub struct HostSecurity {
    identity: Identity,
    name: String,
    trusted: Mutex<Trusted>,
    /// Where the paired list is saved (none: kept in memory, tests).
    trusted_path: Option<PathBuf>,
    pairing: Mutex<Option<PairingWindow>>,
}

impl std::fmt::Debug for HostSecurity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HostSecurity({}, {:?})", self.name, self.identity.public)
    }
}

impl HostSecurity {
    pub fn new(
        identity: Identity,
        name: &str,
        trusted: Trusted,
        trusted_path: Option<PathBuf>,
    ) -> Self {
        Self {
            identity,
            name: name.to_owned(),
            trusted: Mutex::new(trusted),
            trusted_path,
            pairing: Mutex::new(None),
        }
    }

    /// Key and paired clients from `dir` (`host.json`, `clients.json`),
    /// created on first use.
    pub fn load(dir: &Path, name: &str) -> anyhow::Result<Self> {
        let identity =
            Identity::load_or_create(&dir.join("host.json")).map_err(anyhow::Error::msg)?;
        let path = dir.join("clients.json");
        let trusted = Trusted::load(&path).map_err(anyhow::Error::msg)?;
        Ok(Self::new(identity, name, trusted, Some(path)))
    }

    pub fn public_key(&self) -> PublicKey {
        self.identity.public
    }

    /// Lets one client pair with `pin` within [`PAIRING_OPEN_FOR`].
    pub fn open_pairing(&self, pin: &str) {
        *self.pairing.lock().unwrap_or_else(|e| e.into_inner()) = Some(PairingWindow {
            pin: pin.to_owned(),
            until: Instant::now() + PAIRING_OPEN_FOR,
            attempts_left: PAIRING_ATTEMPTS,
        });
    }

    pub fn paired(&self) -> Vec<Peer> {
        self.trusted
            .lock()
            .map(|t| t.peers.clone())
            .unwrap_or_default()
    }

    fn is_paired(&self, key: &PublicKey) -> Option<Peer> {
        self.trusted.lock().ok()?.get(key).cloned()
    }

    fn add(&self, peer: Peer) {
        let mut t = self.trusted.lock().unwrap_or_else(|e| e.into_inner());
        t.add(peer);
        if let Some(path) = &self.trusted_path
            && let Err(e) = t.save(path)
        {
            log::error!("saving the paired clients: {e}");
        }
    }

    /// The PIN if pairing is open; counts the attempt.
    fn pairing_attempt(&self) -> Result<String, RejectReason> {
        let mut w = self.pairing.lock().unwrap_or_else(|e| e.into_inner());
        match w.as_mut() {
            Some(p) if p.until > Instant::now() && p.attempts_left > 0 => {
                p.attempts_left -= 1;
                Ok(p.pin.clone())
            }
            Some(p) if p.attempts_left == 0 => {
                *w = None;
                Err(RejectReason::TooManyAttempts)
            }
            _ => {
                *w = None;
                Err(RejectReason::PairingClosed)
            }
        }
    }

    pub fn close_pairing(&self) {
        *self.pairing.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// How long pairing stays open, if it is.
    pub fn pairing_remaining(&self) -> Option<Duration> {
        let w = self.pairing.lock().unwrap_or_else(|e| e.into_inner());
        w.as_ref()
            .filter(|p| p.attempts_left > 0)
            .and_then(|p| p.until.checked_duration_since(Instant::now()))
    }

    /// Forgets a paired device, by name or key fingerprint.
    pub fn unpair(&self, device: &str) -> bool {
        let mut t = self.trusted.lock().unwrap_or_else(|e| e.into_inner());
        let Some(key) = t
            .peers
            .iter()
            .find(|p| p.name == device || p.key.fingerprint() == device)
            .map(|p| p.key)
        else {
            return false;
        };
        t.remove(&key);
        if let Some(path) = &self.trusted_path
            && let Err(e) = t.save(path)
        {
            log::error!("saving the paired clients: {e}");
        }
        true
    }
}

/// A pairing in progress with one client (retransmissions get the same
/// answers).
struct PairingExchange {
    from: SocketAddr,
    msg1: Vec<u8>,
    msg2: Vec<u8>,
    state: Option<HostPairing>,
    /// Message 3 and our answer, once paired.
    done: Option<(Vec<u8>, Vec<u8>)>,
}

pub struct HostAgent {
    cfg: HostConfig,
    socket: Arc<UdpSocket>,
    stats: Arc<HostStats>,
    /// The current session, for the control socket.
    status: control::StatusCell,
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
            status: Arc::default(),
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
        let mut opened = [0u8; MAX_DATAGRAM];
        let mut out = [0u8; MAX_DATAGRAM];
        let mut session: Option<Session> = None;
        let mut pairing: Option<PairingExchange> = None;
        // Newest handshake per client (its clock, ms): older ones are replays.
        let mut handshake_times: HashMap<PublicKey, u64> = HashMap::new();
        let control_stop = Arc::new(AtomicBool::new(false));
        let control = match (&self.cfg.control, &self.cfg.security) {
            (Some(path), Some(sec)) => {
                let t =
                    control::serve(path, sec.clone(), self.status.clone(), control_stop.clone())
                        .with_context(|| format!("control socket {}", path.display()))?;
                log::info!("control socket {}", path.display());
                Some(t)
            }
            _ => None,
        };

        while !stop.load(Ordering::Relaxed) {
            if session
                .as_ref()
                .is_some_and(|s| s.last_seen.elapsed() > self.cfg.client_timeout)
            {
                let s = session.take().unwrap();
                log::info!("session {:08x}: client timed out", s.params.session_id);
                s.stop();
                self.set_status(None, "");
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
            let outer = match Packet::decode(&buf[..len]) {
                Ok(p) => p,
                Err(e) => {
                    log::debug!("{from}: dropping datagram: {e}");
                    continue;
                }
            };
            let from_peer = session.as_ref().is_some_and(|s| s.peer == from);
            // Sealed packets of the current session are opened and handled
            // like plain ones; they are the only authentic packets of a
            // secure session.
            let (packet, authentic) = match outer {
                Packet::Sealed(h, sealed) => {
                    let Some(s) = session.as_ref() else { continue };
                    let Some(crypto) = s.link.crypto.as_ref() else {
                        continue;
                    };
                    if !from_peer || h.session_id != s.params.session_id {
                        continue;
                    }
                    let n = match crypto.open(h.counter, sealed, &mut opened) {
                        Ok(n) => n,
                        Err(e) => {
                            log::debug!("{from}: dropping sealed packet: {e}");
                            continue;
                        }
                    };
                    match Packet::decode(&opened[..n]) {
                        Ok(inner) => (inner, true),
                        Err(_) => continue,
                    }
                }
                p => (p, from_peer && self.cfg.security.is_none()),
            };
            if authentic && let Some(s) = session.as_mut() {
                s.last_seen = Instant::now();
            }

            match packet {
                Packet::Hello(hello) if self.cfg.security.is_none() => {
                    if !from_peer {
                        if let Some(old) = session.take() {
                            log::info!("session {:08x}: replaced by {from}", old.params.session_id);
                            old.stop();
                        }
                        let params = self.negotiate(&hello);
                        match self.start_session(params, from, None) {
                            Ok((s, _)) => {
                                log_session(&s, &from.to_string());
                                self.set_status(Some(&s), &from.to_string());
                                session = Some(s);
                            }
                            Err(e) => {
                                // No ack: the client keeps asking and times out.
                                log::error!("session {:08x}: {e:#}", params.session_id);
                                continue;
                            }
                        }
                    }
                    let Some(s) = session.as_ref() else { continue };
                    let n = self.hello_ack(&s.params, s.codec).encode(&mut out);
                    let _ = self.socket.send_to(&out[..n], from);
                }
                Packet::Handshake(h) if !h.reply => {
                    self.on_handshake(
                        h.message,
                        from,
                        &mut session,
                        &mut handshake_times,
                        &mut out,
                    );
                }
                Packet::Pair(p) => self.on_pair(p, from, &mut pairing, &mut out),
                Packet::ClockPing(ping) => {
                    let pong = ClockPong {
                        seq: ping.seq,
                        client_send_us: ping.client_send_us,
                        host_recv_us: recv_us,
                        host_send_us: now_us(),
                    };
                    let n = pong.encode(&mut out);
                    if self.cfg.security.is_none() {
                        let _ = self.socket.send_to(&out[..n], from);
                    } else if authentic && let Some(s) = session.as_ref() {
                        let _ = s.link.send(&out[..n]);
                    }
                }
                Packet::Feedback(fb) if authentic => {
                    if let Some(s) = session.as_mut()
                        && fb.session_id == s.params.session_id
                    {
                        on_feedback(s, &fb);
                    }
                }
                Packet::Input(h, body) if authentic => {
                    let Some(s) = session.as_mut() else { continue };
                    if h.session_id != s.params.session_id {
                        continue;
                    }
                    s.apply_input(body);
                    let ack = InputAck {
                        session_id: s.params.session_id,
                        seq: s.input_seen.last().unwrap_or(0),
                    };
                    let n = ack.encode(&mut out);
                    let _ = s.link.send(&out[..n]);
                }
                Packet::Bye(bye) if authentic => {
                    if let Some(s) = session.take() {
                        if bye.session_id == s.params.session_id {
                            log::info!("session {:08x}: closed by client", s.params.session_id);
                            s.stop();
                            self.set_status(None, "");
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
            let _ = s.link.send(&out[..n]);
            s.stop();
        }
        control_stop.store(true, Ordering::Relaxed);
        if let Some(t) = control {
            let _ = t.join();
        }
        Ok(())
    }

    /// What the control socket reports; `None` when no session runs.
    fn set_status(&self, s: Option<&Session>, client: &str) {
        let status = s.map(|s| control::SessionStatus {
            client: client.to_owned(),
            address: s.peer.to_string(),
            width: s.params.width,
            height: s.params.height,
            fps: s.params.fps,
            encrypted: s.link.crypto.is_some(),
            since: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        });
        if let Ok(mut cell) = self.status.lock() {
            *cell = status;
        }
    }

    /// A client's first handshake message: a paired client gets a session
    /// (encrypted from here on), anyone else a "not paired" hint.
    fn on_handshake(
        &self,
        msg: &[u8],
        from: SocketAddr,
        session: &mut Option<Session>,
        times: &mut HashMap<PublicKey, u64>,
        out: &mut [u8],
    ) {
        let Some(sec) = &self.cfg.security else {
            return;
        };
        // A retransmission of the handshake that started this session.
        if let Some(s) = session.as_ref()
            && s.peer == from
            && let Some((first, reply)) = &s.handshake
            && first.as_slice() == msg
        {
            let n = Handshake {
                reply: true,
                message: reply,
            }
            .encode(out);
            let _ = self.socket.send_to(&out[..n], from);
            return;
        }
        let responder = match Responder::read(&sec.identity, msg) {
            Ok(r) => r,
            Err(e) => {
                log::debug!("{from}: handshake: {e}");
                return;
            }
        };
        let Some(client) = sec.is_paired(&responder.client) else {
            log::warn!(
                "{from}: refused an unpaired device ({}); pair it with --pair",
                responder.client.fingerprint()
            );
            let n = RejectReason::NotPaired.encode(out);
            let _ = self.socket.send_to(&out[..n], from);
            return;
        };
        let Some((hello, sent_ms)) = Hello::decode_with_time(&responder.payload) else {
            return;
        };
        if times.get(&client.key).is_some_and(|&t| sent_ms <= t) {
            log::warn!("{from}: replayed handshake of {} dropped", client.name);
            return;
        }
        times.insert(client.key, sent_ms);
        if let Some(old) = session.take() {
            log::info!("session {:08x}: replaced by {from}", old.params.session_id);
            old.stop();
        }
        let params = self.negotiate(&hello);
        match self.start_session(params, from, Some(responder)) {
            Ok((mut s, Some(reply))) => {
                log_session(&s, &format!("{} ({from})", client.name));
                self.set_status(Some(&s), &client.name);
                let n = Handshake {
                    reply: true,
                    message: &reply,
                }
                .encode(out);
                let _ = self.socket.send_to(&out[..n], from);
                s.handshake = Some((msg.to_vec(), reply));
                *session = Some(s);
            }
            Ok((s, None)) => s.stop(),
            Err(e) => log::error!("session {:08x}: {e:#}", params.session_id),
        }
    }

    /// Pairing messages (only while pairing mode is open).
    fn on_pair(
        &self,
        p: Pair<'_>,
        from: SocketAddr,
        ex: &mut Option<PairingExchange>,
        out: &mut [u8],
    ) {
        let Some(sec) = &self.cfg.security else {
            return;
        };
        let send = |step: u8, message: &[u8], out: &mut [u8]| {
            let n = Pair { step, message }.encode(out);
            let _ = self.socket.send_to(&out[..n], from);
        };
        match p.step {
            1 => {
                if let Some(e) = ex.as_ref()
                    && e.from == from
                    && e.msg1 == p.message
                {
                    send(2, &e.msg2, out);
                    return;
                }
                let pin = match sec.pairing_attempt() {
                    Ok(pin) => pin,
                    Err(reason) => {
                        let n = reason.encode(out);
                        let _ = self.socket.send_to(&out[..n], from);
                        return;
                    }
                };
                match HostPairing::respond(&pin, &sec.identity, &sec.name, p.message) {
                    Ok((state, msg2)) => {
                        send(2, &msg2, out);
                        *ex = Some(PairingExchange {
                            from,
                            msg1: p.message.to_vec(),
                            msg2,
                            state: Some(state),
                            done: None,
                        });
                    }
                    Err(e) => log::debug!("{from}: pairing: {e}"),
                }
            }
            3 => {
                let Some(e) = ex.as_mut().filter(|e| e.from == from) else {
                    return;
                };
                if let Some((msg3, msg4)) = &e.done {
                    if msg3.as_slice() == p.message {
                        send(4, msg4, out);
                    }
                    return;
                }
                let Some(state) = e.state.take() else { return };
                match state.finish(p.message) {
                    Ok((peer, msg4)) => {
                        log::info!(
                            "paired with {} ({}, key {})",
                            peer.name,
                            from.ip(),
                            peer.key.fingerprint()
                        );
                        sec.add(peer);
                        sec.close_pairing();
                        send(4, &msg4, out);
                        e.done = Some((p.message.to_vec(), msg4));
                    }
                    Err(err) => log::warn!("pairing with {from} failed: {err}"),
                }
            }
            _ => {}
        }
    }

    /// The client's wishes capped by the host's limits. A width, height or
    /// bitrate of 0 is resolved when the session starts: the size of the
    /// captured screen, and a bitrate for that size.
    fn negotiate(&self, hello: &Hello) -> SessionParams {
        let pick = |want: u16, max: u16| if want == 0 { max } else { want.min(max) };
        SessionParams {
            session_id: new_session_id(),
            width: hello.width.min(self.cfg.max_width) & !1,
            height: hello.height.min(self.cfg.max_height) & !1,
            fps: pick(hello.fps, self.cfg.max_fps).max(1),
            bitrate_kbps: hello.bitrate_kbps.min(self.cfg.max_bitrate_kbps),
        }
    }

    /// Fills in what the client left to the host (see [`Self::negotiate`]).
    fn resolve(&self, mut p: SessionParams, native: (u32, u32)) -> SessionParams {
        if p.width == 0 || p.height == 0 {
            let (w, h) = fit(native, (self.cfg.max_width, self.cfg.max_height));
            p.width = w;
            p.height = h;
        }
        if p.bitrate_kbps == 0 {
            p.bitrate_kbps = default_bitrate_kbps(p.width, p.height, p.fps)
                .min(self.cfg.max_bitrate_kbps)
                .max(1);
        }
        p
    }

    /// Starts a session. With `secure`, the handshake is answered with the
    /// HelloAck inside and the session's packets are sealed; the answer to
    /// send is returned.
    fn start_session(
        &self,
        params: SessionParams,
        peer: SocketAddr,
        secure: Option<Responder>,
    ) -> anyhow::Result<(Session, Option<Vec<u8>>)> {
        let loss = LossEstimator::default();
        HostStats::bump(&self.stats.sessions);
        let shared = Arc::new(Shared {
            stats: self.stats.clone(),
            running: AtomicBool::new(true),
            keyframe_requested: AtomicBool::new(true),
            cursor_requested: AtomicBool::new(false),
            fec_loss: AtomicU32::new(loss.estimate().to_bits()),
        });
        // Source and encoder come first: if the screen or the GPU is
        // unavailable the session fails before any thread starts. The
        // source tells the size of the screen, for a client that asked for
        // the host's own resolution.
        let source = make_source(&self.cfg.capture, &params)?;
        let params = self.resolve(params, (source.width(), source.height()));
        let source = match &self.cfg.capture {
            // The test pattern is drawn at whatever size the session uses.
            CaptureKind::TestPattern => make_source(&self.cfg.capture, &params)?,
            _ => source,
        };
        let encoder = make_encoder(&self.cfg.encoder, &params)?;
        let codec = encoder.codec();
        let input = make_input(&self.cfg.input, source.screen());
        let (crypto, handshake_reply) = match secure {
            None => (None, None),
            Some(responder) => {
                let mut ack = [0u8; HelloAck::LEN];
                self.hello_ack(&params, codec).encode(&mut ack);
                let (transport, reply) = responder
                    .reply(&ack)
                    .map_err(|e| anyhow::anyhow!("handshake: {e}"))?;
                (Some(Arc::new(transport)), Some(reply))
            }
        };
        let link = Link {
            socket: self.socket.clone(),
            peer,
            session_id: params.session_id,
            crypto,
        };
        let frame_slot = Arc::new(Slot::new());
        let (send_tx, send_rx) = bounded::<EncodedFrame>(SEND_QUEUE);

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
            let cursor = CursorSender::new(
                link.clone(),
                params.session_id,
                (source.width(), source.height()),
            );
            spawn_hot("capture", move || {
                capture_loop(source, &shared, &slot, &free_rx, &free_tx, cursor);
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
        let audio = match make_audio(self.cfg.audio) {
            Ok(None) => None,
            Ok(Some(source)) => {
                let (shared, link) = (shared.clone(), link.clone());
                let session_id = params.session_id;
                Some(spawn_hot("audio", move || {
                    if let Err(e) = audio_loop(source, &shared, &link, session_id) {
                        log::warn!("audio: {e}");
                    }
                })?)
            }
            Err(e) => {
                log::warn!("no sound: {e}");
                None
            }
        };
        let send = {
            let (shared, link) = (shared.clone(), link.clone());
            let pacer = Pacer {
                rate_bytes_per_sec: self.cfg.pace_bytes_per_sec,
                burst: 4,
            };
            let loss = LossSim::new(self.cfg.loss, u64::from(params.session_id) | 1);
            spawn_hot("send", move || {
                if let Err(e) =
                    send_loop(&shared, &send_rx, &free_enc_tx, &link, params, pacer, loss)
                {
                    log::error!("send: {e:#}");
                }
            })?
        };

        let session = Session {
            params,
            codec,
            peer,
            link,
            handshake: None,
            last_seen: Instant::now(),
            shared,
            loss,
            threads: [Some(capture), Some(encode), Some(send), audio]
                .into_iter()
                .flatten()
                .collect(),
            frame_slot,
            input,
            input_seen: Dedup::default(),
            input_errors: RepeatedError::default(),
        };
        Ok((session, handshake_reply))
    }

    fn hello_ack(&self, p: &SessionParams, codec: Codec) -> HelloAck {
        HelloAck {
            session_id: p.session_id,
            width: p.width,
            height: p.height,
            fps: p.fps,
            codec,
        }
    }
}

fn log_session(s: &Session, who: &str) {
    let p = s.params;
    log::info!(
        "session {:08x}: {who} {}x{}@{} {} kbit/s{}",
        p.session_id,
        p.width,
        p.height,
        p.fps,
        p.bitrate_kbps,
        if s.link.crypto.is_some() {
            ", encrypted"
        } else {
            ""
        }
    );
}

fn on_feedback(s: &mut Session, fb: &Feedback) {
    let estimate = s.loss.update(fb.loss_ratio());
    s.shared
        .fec_loss
        .store(estimate.to_bits(), Ordering::Relaxed);
    if fb.request_keyframe {
        s.shared.keyframe_requested.store(true, Ordering::Relaxed);
    }
    if fb.request_cursor {
        s.shared.cursor_requested.store(true, Ordering::Relaxed);
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
    mut cursor: CursorSender,
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
        // Right away, not after encoding: the pointer is the one thing the
        // client can show before the frame it belongs to arrives.
        if let Some(c) = &frame.cursor {
            let asked = shared.cursor_requested.swap(false, Ordering::Relaxed);
            cursor.send(c, asked);
        }
        if let Some(skipped) = slot.put(frame) {
            let _ = free_tx.send(skipped);
        }
    }
    slot.close();
}

/// An unchanged cursor image is sent again, so that a lost piece heals:
/// soon after a change (the first copy of a session can even arrive
/// before the client knows the session), then less often, up to every 2 s.
const CURSOR_SHAPE_FIRST_REPEAT: Duration = Duration::from_millis(100);
const CURSOR_SHAPE_REPEAT: Duration = Duration::from_secs(2);

/// Sends the pointer next to the video: its position with every captured
/// frame, its image when it changes and every [`CURSOR_SHAPE_REPEAT`].
struct CursorSender {
    link: Link,
    session_id: u32,
    screen: (u16, u16),
    /// Serial of the image last sent, when, and the wait for the repeat.
    sent: Option<(u32, Instant, Duration)>,
    warned: bool,
}

impl CursorSender {
    fn new(link: Link, session_id: u32, screen: (u32, u32)) -> Self {
        let clamp = |v: u32| v.min(u32::from(u16::MAX)) as u16;
        Self {
            link,
            session_id,
            screen: (clamp(screen.0), clamp(screen.1)),
            sent: None,
            warned: false,
        }
    }

    /// `asked`: the client said it lacks the image; send it now.
    fn send(&mut self, c: &CursorState, asked: bool) {
        let mut buf = [0u8; MAX_DATAGRAM];
        let img = &c.image;
        let fits = (1..=u32::from(MAX_CURSOR_SIZE)).contains(&img.width)
            && (1..=u32::from(MAX_CURSOR_SIZE)).contains(&img.height)
            && img.pixels.len() == (img.width * img.height * 4) as usize;
        if !fits {
            if !self.warned {
                log::warn!("pointer image {}×{} cannot be sent", img.width, img.height);
                self.warned = true;
            }
            return;
        }
        let repeat = match self.sent {
            // Asked for: right away (feedback comes every 100 ms at most).
            Some((serial, _, wait)) if serial == c.serial && asked => Some(wait),
            Some((serial, at, wait)) if serial == c.serial => {
                (at.elapsed() >= wait).then(|| (wait * 2).min(CURSOR_SHAPE_REPEAT))
            }
            _ => Some(CURSOR_SHAPE_FIRST_REPEAT),
        };
        if let Some(next_wait) = repeat {
            for (i, chunk) in img.pixels.chunks(CURSOR_CHUNK).enumerate() {
                let n = CursorShape {
                    session_id: self.session_id,
                    serial: c.serial,
                    width: img.width as u16,
                    height: img.height as u16,
                    offset: (i * CURSOR_CHUNK) as u32,
                }
                .encode(chunk, &mut buf);
                let _ = self.link.send(&buf[..n]);
            }
            self.sent = Some((c.serial, Instant::now(), next_wait));
        }
        let n = Cursor {
            session_id: self.session_id,
            visible: c.visible,
            shape_serial: c.serial,
            x: c.x,
            y: c.y,
            screen_width: self.screen.0,
            screen_height: self.screen.1,
        }
        .encode(&mut buf);
        let _ = self.link.send(&buf[..n]);
    }
}

/// Opus bitrate for the desktop's sound: transparent for music.
const AUDIO_BITRATE: u32 = 128_000;

fn make_audio(kind: AudioKind) -> Result<Option<Box<dyn AudioSource>>, String> {
    match kind {
        AudioKind::Off => Ok(None),
        AudioKind::Tone => Ok(Some(Box::new(Tone::new(440.0)))),
        AudioKind::Desktop => Ok(Some(Box::new(fernsicht_audio::pulse::Capture::open()?))),
    }
}

/// Captures, encodes and sends 5 ms frames until the session ends. Each
/// packet repeats the previous frame, so one lost packet leaves no gap.
fn audio_loop(
    mut source: Box<dyn AudioSource>,
    shared: &Shared,
    link: &Link,
    session_id: u32,
) -> Result<(), String> {
    let mut encoder = fernsicht_audio::opus::Encoder::new(AUDIO_BITRATE)?;
    let mut pcm: AudioFrame = [0; fernsicht_audio::FRAME_SAMPLES * fernsicht_audio::CHANNELS];
    let mut current = [0u8; MAX_AUDIO_FRAME];
    let mut previous = Vec::with_capacity(MAX_AUDIO_FRAME);
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut seq = 0u32;
    while shared.running.load(Ordering::Acquire) {
        let captured_us = source.next_frame(&mut pcm)?;
        let n = encoder.encode(&pcm, &mut current)?;
        let len = AudioHeader {
            session_id,
            seq,
            capture_us: captured_us,
        }
        .encode(&current[..n], &previous, &mut buf);
        let _ = link.send(&buf[..len]);
        HostStats::bump(&shared.stats.audio_frames_sent);
        previous.clear();
        previous.extend_from_slice(&current[..n]);
        seq = seq.wrapping_add(1);
    }
    Ok(())
}

fn make_input(
    kind: &InputKind,
    screen: Option<fernsicht_capture::ScreenInfo>,
) -> Option<Box<dyn InputSink>> {
    match kind {
        InputKind::Off => None,
        InputKind::Record(r) => Some(Box::new(r.clone())),
        InputKind::Uinput => {
            let area = screen
                .map(|s| layout::input_area(&s.connector, &s.active))
                .unwrap_or_default();
            match Uinput::open(area) {
                Ok(u) => Some(Box::new(u)),
                Err(e) => {
                    log::error!("no input: {e}");
                    None
                }
            }
        }
    }
}

/// Size of the test pattern when the client leaves the size to the host.
pub const TEST_PATTERN_SIZE: (u32, u32) = (1920, 1080);

/// `native` scaled down to fit `max` (aspect ratio kept), even sizes.
fn fit(native: (u32, u32), max: (u16, u16)) -> (u16, u16) {
    let (w, h) = (native.0.max(2) as f64, native.1.max(2) as f64);
    let s = (f64::from(max.0) / w).min(f64::from(max.1) / h).min(1.0);
    let even = |v: f64| ((v.round() as u32).max(2) & !1).min(u32::from(u16::MAX) & !1) as u16;
    (even(w * s), even(h * s))
}

/// About what 20 Mbit/s are for 1080p60, the same bits per pixel for any
/// size and rate. Desktop text needs that much to stay sharp.
fn default_bitrate_kbps(width: u16, height: u16, fps: u16) -> u32 {
    let pixels_per_sec = f64::from(width) * f64::from(height) * f64::from(fps);
    let per_1080p60 = 1920.0 * 1080.0 * 60.0;
    (20_000.0 * pixels_per_sec / per_1080p60)
        .round()
        .clamp(1_000.0, 200_000.0) as u32
}

fn make_source(kind: &CaptureKind, p: &SessionParams) -> anyhow::Result<Box<dyn FrameSource>> {
    match kind {
        CaptureKind::TestPattern => {
            let (w, h) = if p.width == 0 || p.height == 0 {
                TEST_PATTERN_SIZE
            } else {
                (u32::from(p.width), u32::from(p.height))
            };
            Ok(Box::new(TestPattern::new(w, h, u32::from(p.fps))))
        }
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

const NVIDIA: u32 = 0x10de;

/// The hardware encoder for the GPUs found (render node, PCI vendor), in
/// order: VAAPI on AMD/Intel, NVENC on NVIDIA, as far as this build
/// supports them (`vaapi`, `nvenc`).
pub fn choose_encoder(gpus: &[(String, u32)], vaapi: bool, nvenc: bool) -> Option<EncoderKind> {
    gpus.iter().find_map(|(node, vendor)| match *vendor {
        NVIDIA if nvenc => Some(EncoderKind::Nvenc { gpu: 0 }),
        NVIDIA => None,
        _ if vaapi => Some(EncoderKind::Vaapi {
            render_node: node.clone(),
        }),
        _ => None,
    })
}

/// [`choose_encoder`] for this machine's GPUs.
pub fn auto_encoder() -> anyhow::Result<EncoderKind> {
    let mut gpus: Vec<(String, u32)> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !name.starts_with("renderD") {
                return None;
            }
            let vendor = std::fs::read_to_string(e.path().join("device/vendor")).ok()?;
            let vendor = u32::from_str_radix(vendor.trim().trim_start_matches("0x"), 16).ok()?;
            Some((format!("/dev/dri/{name}"), vendor))
        })
        .collect();
    gpus.sort();
    let kind = choose_encoder(&gpus, cfg!(feature = "vaapi"), cfg!(feature = "nvidia"))
        .ok_or_else(|| {
            anyhow::anyhow!("no GPU this build can encode with (found {gpus:x?}); pick --encoder")
        })?;
    log::info!("encoder: {kind:?}");
    Ok(kind)
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
        #[cfg(feature = "nvidia")]
        EncoderKind::Nvenc { gpu } => Ok(Box::new(fernsicht_codec::nvidia::NvencEncoder::new(
            &fernsicht_codec::nvidia::NvencConfig {
                gpu: *gpu,
                width: u32::from(p.width),
                height: u32::from(p.height),
                fps: u32::from(p.fps),
                bitrate_kbps: p.bitrate_kbps,
            },
        )?)),
        #[cfg(not(feature = "nvidia"))]
        EncoderKind::Nvenc { .. } => {
            anyhow::bail!("this build has no NVENC support (cargo feature \"nvidia\")")
        }
    }
}

/// Collapses an error that repeats every frame into one log line per second.
#[derive(Default)]
struct RepeatedError {
    last: String,
    repeats: u64,
    since: Option<Instant>,
}

impl RepeatedError {
    /// The line to log for this occurrence, if any.
    fn report(&mut self, msg: String, now: Instant) -> Option<String> {
        if msg != self.last || self.since.is_none() {
            self.last = msg;
            self.repeats = 0;
            self.since = Some(now);
            return Some(self.last.clone());
        }
        self.repeats += 1;
        let since = self.since.expect("set above");
        if now.duration_since(since) < Duration::from_secs(1) {
            return None;
        }
        let line = format!(
            "{} (another {}× in the last second)",
            self.last, self.repeats
        );
        self.repeats = 0;
        self.since = Some(now);
        Some(line)
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
    let mut errors = RepeatedError::default();
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
                if let Some(line) = errors.report(e.to_string(), Instant::now()) {
                    log::warn!("encode: {line}");
                }
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
    link: &Link,
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
            match link.send(p) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_size_is_fitted_into_the_limits() {
        assert_eq!(fit((2560, 1440), (3840, 2160)), (2560, 1440));
        assert_eq!(fit((2560, 1440), (1920, 1080)), (1920, 1080));
        // Aspect ratio kept, even sizes.
        assert_eq!(fit((2560, 1600), (1920, 1080)), (1728, 1080));
        assert_eq!(fit((1366, 768), (3840, 2160)), (1366, 768));
        assert_eq!(fit((1365, 767), (3840, 2160)), (1364, 766));
        assert_eq!(fit((0, 0), (640, 480)), (2, 2));
    }

    #[test]
    fn the_encoder_follows_the_gpu() {
        let amd = ("/dev/dri/renderD128".to_owned(), 0x1002);
        let nv = ("/dev/dri/renderD129".to_owned(), NVIDIA);
        let vaapi = |n: &str| {
            Some(EncoderKind::Vaapi {
                render_node: n.into(),
            })
        };
        assert_eq!(
            choose_encoder(std::slice::from_ref(&amd), true, true),
            vaapi("/dev/dri/renderD128")
        );
        assert_eq!(
            choose_encoder(std::slice::from_ref(&nv), true, true),
            Some(EncoderKind::Nvenc { gpu: 0 })
        );
        // The first GPU the build can use.
        assert_eq!(
            choose_encoder(&[nv.clone(), amd.clone()], true, false),
            vaapi("/dev/dri/renderD128")
        );
        assert_eq!(choose_encoder(&[amd], false, true), None);
        assert_eq!(choose_encoder(&[], true, true), None);
        // This machine: whatever GPUs it has, a build without hardware
        // encoders finds none to use.
        let here = auto_encoder();
        if !cfg!(any(feature = "vaapi", feature = "nvidia")) {
            assert!(here.unwrap_err().to_string().contains("--encoder"));
        }
    }

    #[test]
    fn default_bitrate_follows_the_pixel_rate() {
        assert_eq!(default_bitrate_kbps(1920, 1080, 60), 20_000);
        assert_eq!(default_bitrate_kbps(2560, 1440, 60), 35_556);
        assert_eq!(default_bitrate_kbps(1920, 1080, 30), 10_000);
        assert_eq!(default_bitrate_kbps(320, 240, 30), 1_000, "floor");
        assert_eq!(default_bitrate_kbps(3840, 2160, 144), 192_000);
    }

    #[test]
    fn repeated_errors_are_collapsed() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut e = RepeatedError::default();
        assert_eq!(e.report("bad".into(), t0).as_deref(), Some("bad"));
        for i in 1..60 {
            assert_eq!(e.report("bad".into(), t0 + ms(i * 16)), None);
        }
        assert_eq!(
            e.report("bad".into(), t0 + ms(1000)).as_deref(),
            Some("bad (another 60× in the last second)")
        );
        assert_eq!(e.report("bad".into(), t0 + ms(1016)), None);
        // A different error is reported at once.
        assert_eq!(
            e.report("worse".into(), t0 + ms(1020)).as_deref(),
            Some("worse")
        );
    }
}
