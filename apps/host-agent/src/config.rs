//! The host's configuration: what it captures, encodes and accepts, its
//! settings file and how it describes itself to clients.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fernsicht_input::Recorder;
use fernsicht_proto::CodecSet;
use serde::{Deserialize, Serialize};

use crate::backends::{NVIDIA, render_nodes};
use crate::security::HostSecurity;

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
    /// `security`. See [`control`](crate::control).
    pub control: Option<PathBuf>,
    /// What the host says about itself when clients look for hosts in the
    /// LAN (it answers only with `security`, which gives name and key).
    pub description: HostDescription,
    /// TCP address for the web viewer (page and API; needs `security`).
    /// `None`: no web viewer.
    pub web: Option<String>,
    /// The built viewer (`web/viewer/dist`) the web server serves.
    pub web_root: Option<PathBuf>,
    /// Settings the desktop's user changes while the host runs.
    pub settings: Arc<HostSettings>,
    /// Where the GPU's clock settings are noted during a session, so a
    /// crashed host can put them back (`gpu-power.json` in the state directory).
    /// `None`: not noted.
    pub power_record: Option<PathBuf>,
}

/// Host settings the desktop's user may change while the host runs (over
/// the control socket), kept in the state directory.
#[derive(Debug, Default)]
pub struct HostSettings {
    gpu_boost: AtomicBool,
    path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct SettingsFile {
    gpu_boost: bool,
}

impl HostSettings {
    /// From `path` (`settings.json`); what is missing or broken takes the
    /// defaults: GPU boost on.
    pub fn load(path: PathBuf) -> Self {
        let file = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<SettingsFile>(&t).ok());
        Self {
            gpu_boost: AtomicBool::new(file.is_none_or(|f| f.gpu_boost)),
            path: Some(path),
        }
    }

    /// Clock the GPU up during sessions ([`power`](crate::power)).
    pub fn gpu_boost(&self) -> bool {
        self.gpu_boost.load(Ordering::Relaxed)
    }

    /// Off for this run only; the stored setting stays (`--no-gpu-boost`).
    pub fn without_gpu_boost(self) -> Self {
        self.gpu_boost.store(false, Ordering::Relaxed);
        Self { path: None, ..self }
    }

    pub fn set_gpu_boost(&self, on: bool) -> std::io::Result<()> {
        self.gpu_boost.store(on, Ordering::Relaxed);
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string(&SettingsFile { gpu_boost: on })
            .map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }
}

/// OS and GPU as shown in clients' host lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostDescription {
    /// E.g. "Bazzite".
    pub os: String,
    /// GPU and encoder, e.g. "Radeon RX 7700 XT / 7800 XT · H.264".
    pub gpu: String,
}

impl HostDescription {
    /// This machine, encoding with `encoder`.
    pub fn of_this_machine(encoder: &EncoderKind) -> Self {
        let node = match encoder {
            EncoderKind::Vaapi { render_node } => Some(render_node.clone()),
            EncoderKind::Nvenc { .. } => render_nodes(Path::new("/sys"))
                .into_iter()
                .find(|(_, v)| *v == NVIDIA)
                .map(|(n, _)| n),
            EncoderKind::Synthetic => None,
        };
        let gpu = match (
            encoder,
            node.and_then(|n| fernsicht_core::sysinfo::gpu_name(&n)),
        ) {
            (EncoderKind::Synthetic, _) => "Testbild".into(),
            (_, Some(name)) => format!("{name} · H.264"),
            (_, None) => "H.264".into(),
        };
        Self {
            os: fernsicht_core::sysinfo::os_name(),
            gpu,
        }
    }
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
    /// typing on this machine takes an explicit choice.
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
    /// The test pattern with the monitor off for the frame slots `off`, as
    /// KMS fails for a monitor in standby, and standing still for the slots
    /// `still`, as a desktop nobody touches (tests).
    #[doc(hidden)]
    TestPatternWith {
        off: std::ops::Range<u64>,
        still: std::ops::Range<u64>,
    },
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
            description: HostDescription::default(),
            web: None,
            web_root: None,
            settings: Arc::default(),
            power_record: None,
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
    /// What the client can decode; the encoder picks the best of it.
    pub codecs: CodecSet,
}
