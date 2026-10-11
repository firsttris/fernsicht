//! What a session is started with: the host, the stream asked for, the
//! decoder, where picture, sound and statistics go.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use fernsicht_proto::{Codec, CodecSet};
use fernsicht_secure::{Identity, PublicKey};

use crate::cursor::PointerShare;
use crate::input::InputHandle;
use crate::monitors::MonitorControl;

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
    /// After a loss, keep the picture going and let the host's encoder
    /// heal it by intra refresh (NVENC hosts), instead of waiting for a
    /// keyframe: smooth instead of sharp.
    pub refresh: bool,
    /// Mouse and keyboard to send to the host; `None` = view only.
    pub input: Option<Arc<InputHandle>>,
    /// Where the host's sound is played.
    pub audio: AudioOutput,
    /// Sound muted (silence is played instead, so the timing stays).
    pub muted: Arc<AtomicBool>,
    /// The host's monitors as it reports them, and switches asked for.
    pub monitors: Arc<MonitorControl>,
    /// The host's pointer, for the window to show as its own.
    pub pointer: Arc<PointerShare>,
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
            refresh: false,
            input: None,
            audio: AudioOutput::Off,
            muted: Arc::default(),
            monitors: Arc::default(),
            pointer: Arc::default(),
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
