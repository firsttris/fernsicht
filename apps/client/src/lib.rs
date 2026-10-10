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

mod cursor;
pub mod discover;

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
use fernsicht_input::InputQueue;
pub use fernsicht_input::{InputEvent, buttons};
use fernsicht_net::{ClockSync, LossSim, Reassembler, ReceiverStats};
use fernsicht_proto::{
    Bye, ClockPing, Codec, CodecSet, Feedback, Handshake, Hello, InputHeader, MAX_DATAGRAM, Packet,
    Pair as PairMsg, RejectReason, SealedHeader, VideoHeader,
};
use fernsicht_render::overlay::{self, StreamInfo};
use fernsicht_render::{HeadlessPresenter, Presenter};
use fernsicht_secure::pairing::{ClientPairing, PairError};
use fernsicht_secure::session::{Initiator, Transport};
pub use fernsicht_secure::{Identity, Peer, PublicKey, Trusted};

use crate::cursor::CursorTracker;

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
    /// Print the overlay on stdout once per second.
    pub print_overlay: OverlayOutput,
    /// Give up when the host has been silent this long.
    pub host_timeout: Duration,
    /// GPU used for hardware decoding (VAAPI).
    pub render_node: String,
    /// Which hardware decoder to use.
    pub decoder: DecoderChoice,
    /// Which video codec to ask the host for.
    pub codec: CodecChoice,
    /// Mouse and keyboard to send to the host; `None` = view only.
    pub input: Option<Arc<InputHandle>>,
    /// Where the host's sound is played.
    pub audio: AudioOutput,
    /// Sound muted (silence is played instead, so the timing stays).
    pub muted: Arc<AtomicBool>,
    /// Our key and the paired host's: the session is authenticated and
    /// encrypted. `None` (the library default) talks plain, for tests; the
    /// program always sets it.
    pub security: Option<Arc<ClientSecurity>>,
    /// Writes the received bitstream here (H.264 Annex B: plays with
    /// `ffplay` or `mpv`). Only frames the decoder gets are written.
    pub record: Option<std::path::PathBuf>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1:47800".into(),
            // The host's screen size and a bitrate to match.
            width: 0,
            height: 0,
            fps: 60,
            bitrate_kbps: 0,
            loss: 0.0,
            duration: None,
            print_overlay: OverlayOutput::Off,
            host_timeout: Duration::from_secs(5),
            render_node: "/dev/dri/renderD128".into(),
            decoder: DecoderChoice::Auto,
            codec: CodecChoice::Auto,
            input: None,
            audio: AudioOutput::Off,
            muted: Arc::default(),
            security: None,
            record: None,
        }
    }
}

/// How the overlay is printed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayOutput {
    #[default]
    Off,
    /// The overlay's text lines (the terminal client).
    Text,
    /// One JSON object per line (the desktop app reads them).
    Json,
}

/// This device's key and the key of the host it connects to (known from
/// pairing).
pub struct ClientSecurity {
    pub identity: Identity,
    pub host: PublicKey,
}

impl std::fmt::Debug for ClientSecurity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClientSecurity(host {})", self.host.fingerprint())
    }
}

/// Sends `packet` to the host: sealed once the session is secure.
fn send_packet(socket: &UdpSocket, crypto: Option<(&Transport, u32)>, packet: &[u8]) {
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

/// Where this device keeps its key and paired hosts:
/// `$XDG_CONFIG_HOME/fernsicht`, else `~/.config/fernsicht`.
pub fn default_state_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("fernsicht")
}

/// The port hosts listen on unless told otherwise.
pub const DEFAULT_PORT: u16 = 47800;

/// `host` with the default port if it has none (`zentrale.local` →
/// `zentrale.local:47800`; IPv6 needs brackets: `[fe80::1]:47800`).
pub fn with_default_port(host: &str) -> String {
    let has_port = match host.rsplit_once(':') {
        Some((h, p)) => p.parse::<u16>().is_ok() && (!h.contains(':') || h.ends_with(']')),
        None => false,
    };
    if has_port {
        host.to_owned()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    }
}

/// The paired host meant by `what`: its name or its address.
pub fn find_host<'a>(trusted: &'a Trusted, what: &str) -> Option<&'a Peer> {
    let addr = with_default_port(what);
    trusted.peers.iter().find(|p| p.name == what).or_else(|| {
        trusted
            .peers
            .iter()
            .find(|p| p.address.as_deref() == Some(addr.as_str()))
    })
}

/// Updates the addresses of paired hosts that were found elsewhere (a new
/// address from DHCP), matched by key. Returns whether anything changed.
pub fn refresh_addresses(trusted: &mut Trusted, found: &[discover::FoundHost]) -> bool {
    let mut changed = false;
    for f in found {
        let addr = f.addr.to_string();
        if let Some(p) = trusted.peers.iter_mut().find(|p| p.key == f.key)
            && p.address.as_deref() != Some(addr.as_str())
        {
            p.address = Some(addr);
            changed = true;
        }
    }
    changed
}

/// How long pairing waits for the host.
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(10);

/// Pairs with the host at `host` using the PIN it shows. Returns the host
/// to remember (with its address).
pub fn pair(host: &str, pin: &str, identity: &Identity, name: &str) -> anyhow::Result<Peer> {
    let socket = fernsicht_net::socket::bind_udp("0.0.0.0:0").context("bind")?;
    socket
        .connect(host)
        .with_context(|| format!("connect {host}"))?;
    socket.set_read_timeout(Some(Duration::from_millis(50)))?;
    let (mut pairing, msg1) = ClientPairing::start(pin, identity, name);
    let mut out = [0u8; MAX_DATAGRAM];
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + PAIR_TIMEOUT;
    // Step 1 until step 2 comes, then step 3 until step 4: each resent
    // every 250 ms, as UDP may lose them.
    let mut sending: (u8, Vec<u8>) = (1, msg1);
    let mut last_sent: Option<Instant> = None;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "no answer from {host} (is the host running, and pairing open: \"fernsicht-host-agent pair\"?)"
            );
        }
        if last_sent.is_none_or(|t| t.elapsed() >= HELLO_INTERVAL) {
            let n = PairMsg {
                step: sending.0,
                message: &sending.1,
            }
            .encode(&mut out);
            let _ = socket.send(&out[..n]);
            last_sent = Some(Instant::now());
        }
        let Ok(n) = socket.recv(&mut buf) else {
            continue;
        };
        match Packet::decode(&buf[..n]) {
            Ok(Packet::Pair(p)) if p.step == 2 && sending.0 == 1 => {
                let msg3 = pairing.on_reply(p.message).map_err(|e| match e {
                    PairError::WrongPin => anyhow::anyhow!("wrong PIN"),
                    other => anyhow::anyhow!("pairing failed: {other}"),
                })?;
                sending = (3, msg3);
                last_sent = None;
            }
            Ok(Packet::Pair(p)) if p.step == 4 && sending.0 == 3 => {
                let mut peer = pairing.on_done(p.message)?;
                peer.address = Some(host.to_owned());
                return Ok(peer);
            }
            Ok(Packet::Reject(r)) => anyhow::bail!("{}", reject_text(r)),
            _ => {}
        }
    }
}

/// What a host's rejection means, for people.
pub fn reject_text(r: RejectReason) -> &'static str {
    match r {
        RejectReason::NotPaired => {
            "this device is not paired with the host (on the host: \"fernsicht-host-agent pair\" shows a PIN; here: \"fernsicht-client pair HOST PIN\")"
        }
        RejectReason::PairingClosed => {
            "the host is not in pairing mode (on the host: \"fernsicht-host-agent pair\")"
        }
        RejectReason::TooManyAttempts => "too many wrong PINs: pairing mode closed; open it again",
    }
}

/// Where the host's sound goes.
#[derive(Clone, Default)]
pub enum AudioOutput {
    /// Not played (the library default: tests stay quiet).
    #[default]
    Off,
    /// The default output (PipeWire/PulseAudio).
    Speakers,
    /// Kept (tests).
    Record(fernsicht_audio::Recorder),
}

impl std::fmt::Debug for AudioOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AudioOutput::Off => "Off",
            AudioOutput::Speakers => "Speakers",
            AudioOutput::Record(_) => "Record",
        })
    }
}

/// The window's side of input: events go in here, the network thread
/// sends them reliably (resent until the host acknowledges).
#[derive(Debug, Default)]
pub struct InputHandle {
    queue: Mutex<InputQueue>,
    /// The streamed screen's size, once the host said it.
    stream: Mutex<Option<(u32, u32)>>,
}

impl InputHandle {
    pub fn push(&self, event: InputEvent) {
        if let Ok(mut q) = self.queue.lock() {
            q.push(event);
        }
    }

    /// Lets go of every key and button held (window lost focus or closes).
    pub fn release_all(&self) {
        if let Ok(mut q) = self.queue.lock() {
            q.release_all();
        }
    }

    /// The pointer at `pos` in a window of `window` pixels, if it is on
    /// the picture.
    pub fn pointer_at(&self, pos: (f64, f64), window: (u32, u32)) -> Option<InputEvent> {
        let stream = (*self.stream.lock().ok()?)?;
        let (x, y) = window_to_stream(pos, window, stream)?;
        Some(InputEvent::MouseAbs { x, y })
    }

    fn set_stream(&self, size: (u32, u32)) {
        if let Ok(mut s) = self.stream.lock() {
            *s = Some(size);
        }
    }
}

/// Where a window position falls on the stream (shown letterboxed in the
/// window), as 0..=65535 each way; `None` on the black bars.
pub fn window_to_stream(
    pos: (f64, f64),
    window: (u32, u32),
    stream: (u32, u32),
) -> Option<(u16, u16)> {
    let [sx, sy] = fernsicht_render::letterbox_size(stream, window);
    let (ww, wh) = (f64::from(window.0.max(1)), f64::from(window.1.max(1)));
    // The picture's size and top-left corner in window pixels.
    let (pw, ph) = (ww * f64::from(sx), wh * f64::from(sy));
    let (left, top) = ((ww - pw) / 2.0, (wh - ph) / 2.0);
    let (u, v) = ((pos.0 - left) / pw, (pos.1 - top) / ph);
    if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
        return None;
    }
    let q = |t: f64| (t * f64::from(u16::MAX)).round() as u16;
    Some((q(u), q(v)))
}

/// The video codec a client asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodecChoice {
    /// What this machine decodes in hardware (AV1, HEVC) and H.264; the
    /// host picks the best it can encode.
    #[default]
    Auto,
    H264,
    Hevc,
    Av1,
}

/// What to offer the host for `choice`, given which codecs the hardware
/// decodes (`decodes`).
pub fn offered_codecs(choice: CodecChoice, decodes: impl Fn(Codec) -> bool) -> CodecSet {
    match choice {
        CodecChoice::H264 => CodecSet::H264,
        CodecChoice::Hevc => CodecSet::of(&[Codec::Hevc]),
        CodecChoice::Av1 => CodecSet::of(&[Codec::Av1]),
        // H.264 always: it is what every host encodes, and the decoder is
        // looked for only once the stream arrives. The others only where
        // the hardware decodes them: no decoding on the CPU.
        CodecChoice::Auto => [Codec::Hevc, Codec::Av1]
            .into_iter()
            .filter(|&c| decodes(c))
            .fold(CodecSet::H264, CodecSet::with),
    }
}

/// Hardware decoder for H.264 and HEVC streams.
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
    /// Pointer positions received, and pointer images completed.
    pub cursor_positions: u64,
    pub cursor_shapes: u64,
    pub audio: AudioSummary,
}

/// How the sound went.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioSummary {
    /// 5 ms frames played from what arrived.
    pub played: u64,
    /// Frames lost and bridged by the decoder.
    pub concealed: u64,
    /// Frames dropped to keep the delay down.
    pub dropped: u64,
    /// From the host's speakers to ours, average (µs).
    pub delay_us: u64,
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
    let cursor = Arc::new(Mutex::new(CursorTracker::default()));
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
        let cursor = cursor.clone();
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
                &cursor,
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
    // Sound plays on its own thread, paced by the sound card.
    let (audio_tx, audio_rx) = bounded::<AudioPacket>(64);
    let audio = match cfg.audio.clone() {
        AudioOutput::Off => None,
        out => {
            let muted = cfg.muted.clone();
            Some(spawn_hot("audio", move || {
                audio_loop(&audio_rx, &out, &muted)
            })?)
        }
    };
    let pipe = Pipe {
        audio_tx: audio.as_ref().map(|_| &audio_tx),
        decode_tx: &decode_tx,
        free_rx: &free_rx,
        free_tx: &free_tx,
        need_keyframe: &need_keyframe,
        cursor: &cursor,
    };
    let net = network_loop(&cfg, &socket, &stop, &pipe, &info);
    // Closing the queue ends the present thread once it has drained.
    drop(decode_tx);
    drop(audio_tx);
    let present = presenter.join().expect("present thread panicked");
    let audio = audio
        .map(|t| t.join().expect("audio thread panicked"))
        .unwrap_or_default();
    let net = net?;
    let (cursor_positions, cursor_shapes) = cursor
        .lock()
        .map(|t| (t.positions, t.shapes))
        .unwrap_or_default();

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
        cursor_positions,
        cursor_shapes,
        audio,
    })
}

/// The network thread's ends of the decode queue.
struct Pipe<'a> {
    /// `None` without sound output.
    audio_tx: Option<&'a Sender<AudioPacket>>,
    decode_tx: &'a Sender<ReceivedFrame>,
    free_rx: &'a Receiver<ReceivedFrame>,
    free_tx: &'a Sender<ReceivedFrame>,
    /// Set by the decoder while its reference chain is broken.
    need_keyframe: &'a AtomicBool,
    cursor: &'a Mutex<CursorTracker>,
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
            let due = input
                .queue
                .lock()
                .ok()
                .and_then(|mut q| q.due(Instant::now()));
            if let Some(events) = due {
                let n = InputHeader::encode(session_id, &events, &mut out);
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
        let Ok(outer) = Packet::decode(&buf[..len]) else {
            continue;
        };
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
                inner
            }
            (Packet::Handshake(h), true) if h.reply => {
                let Some((initiator, _)) = handshake.take() else {
                    continue;
                };
                let (transport, payload) = match initiator.finish(h.message) {
                    Ok(r) => r,
                    Err(e) => anyhow::bail!("handshake with {} failed: {e}", cfg.host),
                };
                let Ok(Packet::HelloAck(ack)) = Packet::decode(&payload) else {
                    anyhow::bail!("handshake with {}: no session in the answer", cfg.host);
                };
                crypto = Some(Arc::new(transport));
                Packet::HelloAck(ack)
            }
            (Packet::Reject(r), true) => anyhow::bail!("{}: {}", cfg.host, reject_text(r)),
            (_, true) => continue,
            (p, false) => p,
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
                if let Some(input) = &cfg.input {
                    input.set_stream((u32::from(ack.width), u32::from(ack.height)));
                }
            }
            Packet::ClockPong(pong) => {
                clock.on_pong(&pong, now);
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
                {
                    // Full only if playback stalls; then dropping is right.
                    let _ = tx.try_send(AudioPacket {
                        seq: h.seq,
                        frame: frame.to_vec(),
                        previous: previous.to_vec(),
                        captured_local: h.capture_us as i64 - clock.offset_us(),
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
        send_packet(
            socket,
            crypto.as_deref().map(|t| (t, session_id)),
            &out[..n],
        );
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

impl Hw {
    /// Whether one of the allowed hardware decoders reports `codec`.
    #[allow(unused_variables)]
    fn decodes(&self, codec: Codec) -> bool {
        #[cfg(feature = "vaapi")]
        if matches!(self.decoder, DecoderChoice::Auto | DecoderChoice::Vaapi)
            && fernsicht_codec::vaapi::decodes(&self.render_node, codec)
        {
            return true;
        }
        #[cfg(feature = "nvidia")]
        if matches!(self.decoder, DecoderChoice::Auto | DecoderChoice::Nvdec)
            && fernsicht_codec::nvidia::decodes(0, codec)
        {
            return true;
        }
        false
    }
}

fn make_decoder(codec: Codec, hw: &Hw) -> Result<Box<dyn Decoder>, CodecError> {
    match codec {
        Codec::Synthetic => Ok(Box::new(SyntheticDecoder::default())),
        Codec::H264 | Codec::Hevc | Codec::Av1 => make_hw_decoder(codec, hw),
    }
}

#[allow(unused_variables)]
fn make_hw_decoder(codec: Codec, hw: &Hw) -> Result<Box<dyn Decoder>, CodecError> {
    let label = overlay::codec_label(codec);
    let mut errors = Vec::new();
    if matches!(hw.decoder, DecoderChoice::Auto | DecoderChoice::Vaapi) {
        #[cfg(feature = "vaapi")]
        if !fernsicht_codec::vaapi::decodes(&hw.render_node, codec) {
            errors.push(format!("VAAPI: {} does not decode {label}", hw.render_node));
        } else {
            match fernsicht_codec::vaapi::VaapiDecoder::for_codec(&hw.render_node, codec) {
                Ok(d) => {
                    log::info!("decoding {label} with VAAPI ({})", hw.render_node);
                    return Ok(Box::new(d));
                }
                Err(e) => errors.push(format!("VAAPI: {e}")),
            }
        }
        #[cfg(not(feature = "vaapi"))]
        errors.push("VAAPI: not in this build (cargo feature \"vaapi\")".to_string());
    }
    if matches!(hw.decoder, DecoderChoice::Auto | DecoderChoice::Nvdec) {
        #[cfg(feature = "nvidia")]
        if !fernsicht_codec::nvidia::decodes(0, codec) {
            errors.push(format!("NVDEC: this GPU does not decode {label}"));
        } else {
            match fernsicht_codec::nvidia::NvdecDecoder::for_codec(0, codec) {
                Ok(d) => {
                    log::info!("decoding {label} with NVDEC");
                    return Ok(Box::new(d));
                }
                Err(e) => errors.push(format!("NVDEC: {e}")),
            }
        }
        #[cfg(not(feature = "nvidia"))]
        errors.push("NVDEC: not in this build (cargo feature \"nvidia\")".to_string());
    }
    Err(CodecError::Backend(format!(
        "no {label} decoder: {}",
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
    print_overlay: OverlayOutput,
    hw: &Hw,
    mut record: Option<&mut dyn std::io::Write>,
    presenter: &mut dyn Presenter,
    cursor: &Mutex<CursorTracker>,
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
                        let pointer = cursor.lock().ok().and_then(|t| t.overlay());
                        match presenter.present(&decoded, picture.as_ref(), pointer.as_ref()) {
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
            match print_overlay {
                OverlayOutput::Off => {}
                OverlayOutput::Text => {
                    for line in &lines {
                        println!("{line}");
                    }
                    println!();
                }
                OverlayOutput::Json => println!("{}", overlay::json(&r.stats, &info)),
            }
            last_overlay = Instant::now();
        }
    }
    r
}

/// One audio packet for the playback thread.
struct AudioPacket {
    seq: u32,
    frame: Vec<u8>,
    previous: Vec<u8>,
    /// When the host played it, on our clock (µs).
    captured_local: i64,
}

/// Frames the jitter buffer holds before playing (15 ms), and what the
/// sound server buffers after it (10 ms).
const AUDIO_JITTER_FRAMES: usize = 3;
const AUDIO_OUTPUT_FRAMES: u32 = 2;

fn audio_sink(out: &AudioOutput) -> Option<Box<dyn fernsicht_audio::AudioSink>> {
    match out {
        AudioOutput::Off => None,
        AudioOutput::Record(r) => Some(Box::new(r.clone())),
        AudioOutput::Speakers => {
            match fernsicht_audio::pulse::Playback::open(AUDIO_OUTPUT_FRAMES) {
                Ok(p) => Some(Box::new(p)),
                Err(e) => {
                    log::warn!("no sound: {e}");
                    None
                }
            }
        }
    }
}

/// Plays the host's sound: jitter buffer, Opus decoding with concealment
/// of lost frames, and a sink whose blocking writes pace the loop. The
/// output opens with the first sound from the host. Ends when the network
/// thread hangs up.
fn audio_loop(rx: &Receiver<AudioPacket>, out: &AudioOutput, muted: &AtomicBool) -> AudioSummary {
    use fernsicht_audio::jitter::{Jitter, Next};
    let mut summary = AudioSummary::default();
    let Ok(first) = rx.recv() else {
        return summary;
    };
    let Some(mut sink) = audio_sink(out) else {
        // Drain, so the network thread never blocks on us.
        while rx.recv().is_ok() {}
        return summary;
    };
    let mut decoder = match fernsicht_audio::opus::Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("no sound: {e}");
            return summary;
        }
    };
    let mut jitter = Jitter::new(AUDIO_JITTER_FRAMES);
    let mut pcm: fernsicht_audio::Frame =
        [0; fernsicht_audio::FRAME_SAMPLES * fernsicht_audio::CHANNELS];
    let mut captured: std::collections::BTreeMap<u32, i64> = Default::default();
    let mut started = false;
    let mut delays = (0u64, 0u64);
    take(first, &mut jitter, &mut captured);
    fn take(
        p: AudioPacket,
        jitter: &mut Jitter,
        captured: &mut std::collections::BTreeMap<u32, i64>,
    ) {
        jitter.push(p.seq, &p.frame);
        jitter.push(p.seq.wrapping_sub(1), &p.previous);
        captured.insert(p.seq, p.captured_local);
        while captured.len() > 64 {
            captured.pop_first();
        }
    }
    loop {
        // Wait for sound before starting; once playing, the sink paces us.
        if !started {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(p) => take(p, &mut jitter, &mut captured),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        loop {
            match rx.try_recv() {
                Ok(p) => take(p, &mut jitter, &mut captured),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    summary.played = jitter.played;
                    summary.concealed = jitter.concealed;
                    summary.dropped = jitter.dropped;
                    summary.delay_us = delays.0.checked_div(delays.1).unwrap_or(0);
                    return summary;
                }
            }
        }
        let result = match jitter.pop() {
            Next::Frame(data) => decoder.decode(Some(&data), &mut pcm),
            Next::Lost => decoder.decode(None, &mut pcm),
            Next::Empty if started => decoder.decode(None, &mut pcm),
            Next::Empty => continue,
        };
        if let Err(e) = result {
            log::debug!("audio decode: {e}");
            pcm.fill(0);
        }
        if muted.load(Ordering::Relaxed) {
            pcm.fill(0);
        }
        started = true;
        if let Err(e) = sink.play(&pcm) {
            log::warn!("sound output: {e}");
            break;
        }
        // Delay of the newest frame: from the host's output to ours.
        if let Some((_, &t)) = captured.last_key_value() {
            let queued = jitter.depth() as u64 * u64::from(fernsicht_audio::FRAME_MS) * 1000;
            let d = (now_us() as i64 - t).max(0) as u64 + queued + sink.buffered_us();
            delays = (delays.0 + d, delays.1 + 1);
        }
        // The host's clock runs a little faster than the sound card: the
        // buffer grows. Keep it near the target.
        if jitter.depth() > AUDIO_JITTER_FRAMES + 3 {
            jitter.skip_one();
        }
    }
    summary.played = jitter.played;
    summary.concealed = jitter.concealed;
    summary.dropped = jitter.dropped;
    summary.delay_us = delays.0.checked_div(delays.1).unwrap_or(0);
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_offers_hevc_only_where_it_is_decoded() {
        let none = |_: Codec| false;
        let hevc = |c: Codec| c == Codec::Hevc;
        let all = |_: Codec| true;
        let both = CodecSet::of(&[Codec::H264, Codec::Hevc]);
        // The GTX 1080: HEVC in hardware, no AV1.
        assert_eq!(offered_codecs(CodecChoice::Auto, hevc), both);
        assert_eq!(offered_codecs(CodecChoice::Auto, none), CodecSet::H264);
        assert_eq!(
            offered_codecs(CodecChoice::Auto, all),
            CodecSet::of(&[Codec::H264, Codec::Hevc, Codec::Av1])
        );
        assert_eq!(
            offered_codecs(CodecChoice::Av1, none),
            CodecSet::of(&[Codec::Av1])
        );
        // A choice is passed on as it is.
        assert_eq!(offered_codecs(CodecChoice::H264, hevc), CodecSet::H264);
        assert_eq!(
            offered_codecs(CodecChoice::Hevc, none),
            CodecSet::of(&[Codec::Hevc])
        );
    }

    #[test]
    fn host_addresses_get_the_default_port() {
        assert_eq!(with_default_port("zentrale"), "zentrale:47800");
        assert_eq!(with_default_port("192.168.178.87"), "192.168.178.87:47800");
        assert_eq!(
            with_default_port("192.168.178.87:1234"),
            "192.168.178.87:1234"
        );
        assert_eq!(with_default_port("[fe80::1]:47800"), "[fe80::1]:47800");
        assert_eq!(with_default_port("[fe80::1]"), "[fe80::1]:47800");
        assert_eq!(with_default_port("fe80::1"), "fe80::1:47800");
    }

    #[test]
    fn hosts_are_found_by_name_or_address() {
        let mut t = Trusted::default();
        t.add(Peer {
            name: "zentrale".into(),
            key: Identity::generate().public,
            address: Some("192.168.178.87:47800".into()),
            paired_at: 0,
        });
        assert!(find_host(&t, "zentrale").is_some());
        assert!(find_host(&t, "192.168.178.87").is_some());
        assert!(find_host(&t, "192.168.178.87:47800").is_some());
        assert!(find_host(&t, "192.168.178.88").is_none());
        assert!(find_host(&t, "bazzite").is_none());
    }

    #[test]
    fn found_hosts_update_addresses_by_key() {
        let key = Identity::generate().public;
        let mut t = Trusted::default();
        t.add(Peer {
            name: "zentrale".into(),
            key,
            address: Some("192.168.178.87:47800".into()),
            paired_at: 0,
        });
        let found = |addr: &str, key| discover::FoundHost {
            addr: addr.parse().unwrap(),
            name: "zentrale".into(),
            key,
            pairing: false,
            busy: false,
            os: String::new(),
            gpu: String::new(),
        };
        assert!(!refresh_addresses(
            &mut t,
            &[found("192.168.178.87:47800", key)]
        ));
        // Another host claiming the name changes nothing: the key counts.
        let other = Identity::generate().public;
        assert!(!refresh_addresses(
            &mut t,
            &[found("192.168.178.66:47800", other)]
        ));
        assert!(refresh_addresses(
            &mut t,
            &[found("192.168.178.90:47800", key)]
        ));
        assert_eq!(t.peers[0].address.as_deref(), Some("192.168.178.90:47800"));
    }

    #[test]
    fn the_state_dir_follows_xdg() {
        let d = default_state_dir();
        assert!(d.ends_with("fernsicht"), "{d:?}");
    }

    #[test]
    fn window_positions_map_onto_the_stream() {
        // Same aspect: corners are corners.
        assert_eq!(
            window_to_stream((0.0, 0.0), (1280, 720), (2560, 1440)),
            Some((0, 0))
        );
        assert_eq!(
            window_to_stream((1280.0, 720.0), (1280, 720), (2560, 1440)),
            Some((65535, 65535))
        );
        assert_eq!(
            window_to_stream((640.0, 360.0), (1280, 720), (2560, 1440)),
            Some((32768, 32768))
        );
        // 16:9 stream in a square window: bars above and below.
        let sq = (1000, 1000);
        assert_eq!(
            window_to_stream((500.0, 100.0), sq, (1920, 1080)),
            None,
            "on the bar"
        );
        assert_eq!(
            window_to_stream((500.0, 218.75), sq, (1920, 1080)),
            Some((32768, 0))
        );
        assert_eq!(
            window_to_stream((500.0, 781.25), sq, (1920, 1080)),
            Some((32768, 65535))
        );
        assert_eq!(window_to_stream((-1.0, 500.0), sq, (1920, 1080)), None);
    }

    #[test]
    fn the_input_handle_waits_for_the_stream_size() {
        let h = InputHandle::default();
        assert_eq!(h.pointer_at((10.0, 10.0), (100, 100)), None);
        h.set_stream((100, 100));
        assert!(matches!(
            h.pointer_at((50.0, 50.0), (100, 100)),
            Some(InputEvent::MouseAbs { .. })
        ));
    }

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
