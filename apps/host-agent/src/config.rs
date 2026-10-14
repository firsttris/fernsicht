//! The host's configuration: what it captures, encodes and accepts, its
//! settings file and how it describes itself to clients.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fernsicht_core::colour::Colour;
use fernsicht_input::Recorder;
use fernsicht_proto::{Codec, CodecSet};
use serde::{Deserialize, Serialize};

use crate::backends::{NVIDIA, render_nodes};
use crate::security::HostSecurity;

/// The STUN server asked unless told otherwise.
pub const DEFAULT_STUN: &str = fernsicht_internet::stun::DEFAULT_SERVER;

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
    /// Whose clipboard a session that may control the desktop shares
    /// (none without input).
    pub clipboard: fernsicht_clipboard::Place,
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
    /// STUN servers (`host:port`) the host asks for its public address
    /// when a client connects over the internet.
    pub stun: Vec<String>,
    /// The highest bitrate of a session through a relay, kbit/s: relayed
    /// traffic may cost money.
    pub relay_max_kbps: u32,
}

/// Host settings the desktop's user may change while the host runs (over
/// the control socket), kept in the state directory.
#[derive(Debug, Default)]
pub struct HostSettings {
    gpu_boost: AtomicBool,
    /// The rendezvous service's address, for connections over the internet
    /// (`docs/internet.md`); `None`: LAN and VPN only.
    rendezvous: std::sync::Mutex<Option<String>>,
    /// Ask the router to forward the host's port (PCP, NAT-PMP, UPnP).
    port_mapping: AtomicBool,
    path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct SettingsFile {
    gpu_boost: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rendezvous: Option<String>,
    #[serde(default)]
    port_mapping: bool,
}

impl HostSettings {
    /// From `path` (`settings.json`); what is missing or broken takes the
    /// defaults: GPU boost on, no rendezvous service.
    pub fn load(path: PathBuf) -> Self {
        let file = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<SettingsFile>(&t).ok());
        let rendezvous = file
            .as_ref()
            .and_then(|f| f.rendezvous.clone())
            .filter(|u| fernsicht_internet::rendezvous::is_service_url(u));
        Self {
            gpu_boost: AtomicBool::new(file.as_ref().is_none_or(|f| f.gpu_boost)),
            rendezvous: std::sync::Mutex::new(rendezvous),
            port_mapping: AtomicBool::new(file.as_ref().is_some_and(|f| f.port_mapping)),
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

    /// The rendezvous service for this run only; the stored setting stays
    /// (`--rendezvous`).
    pub fn with_rendezvous(self, url: Option<String>) -> Self {
        *self.rendezvous.lock().unwrap_or_else(|e| e.into_inner()) = url;
        Self { path: None, ..self }
    }

    /// Port forwarding by the router on for this run only; the stored
    /// setting stays (`--port-mapping`).
    pub fn with_port_mapping(self) -> Self {
        self.port_mapping.store(true, Ordering::Relaxed);
        Self { path: None, ..self }
    }

    /// Ask the router to forward the host's port.
    pub fn port_mapping(&self) -> bool {
        self.port_mapping.load(Ordering::Relaxed)
    }

    pub fn set_port_mapping(&self, on: bool) -> std::io::Result<()> {
        self.port_mapping.store(on, Ordering::Relaxed);
        self.save()
    }

    /// The rendezvous service's address, if one is set.
    pub fn rendezvous(&self) -> Option<String> {
        self.rendezvous
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_gpu_boost(&self, on: bool) -> std::io::Result<()> {
        self.gpu_boost.store(on, Ordering::Relaxed);
        self.save()
    }

    /// Sets (or with `None` clears) the rendezvous service; an address that
    /// is not one is refused.
    pub fn set_rendezvous(&self, url: Option<String>) -> std::io::Result<()> {
        let url = url.map(|u| u.trim().trim_end_matches('/').to_owned());
        if let Some(u) = &url
            && !fernsicht_internet::rendezvous::is_service_url(u)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("not a rendezvous service address: {u}"),
            ));
        }
        *self.rendezvous.lock().unwrap_or_else(|e| e.into_inner()) = url;
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string(&SettingsFile {
            gpu_boost: self.gpu_boost(),
            rendezvous: self.rendezvous(),
            port_mapping: self.port_mapping(),
        })
        .map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }
}

/// OS and GPU as shown in clients' host lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostDescription {
    /// E.g. "Bazzite".
    pub os: String,
    /// GPU and the best codec it encodes, e.g. "Radeon RX 7700 XT / 7800
    /// XT · AV1".
    pub gpu: String,
}

impl HostDescription {
    /// This machine, encoding with `encoder`.
    pub fn of_this_machine(encoder: &EncoderKind) -> Self {
        let node = match encoder {
            EncoderKind::Vaapi { render_node } | EncoderKind::Vulkan { render_node } => {
                Some(render_node.clone())
            }
            EncoderKind::Nvenc { .. } => render_nodes(Path::new("/sys"))
                .into_iter()
                .find(|(_, v)| *v == NVIDIA)
                .map(|(n, _)| n),
            EncoderKind::Synthetic => None,
        };
        let gpu = match encoder {
            EncoderKind::Synthetic => "Testbild".into(),
            _ => describe_gpu(
                node.and_then(|n| fernsicht_core::sysinfo::gpu_name(&n)),
                best_codec(encoder),
            ),
        };
        Self {
            os: fernsicht_core::sysinfo::os_name(),
            gpu,
        }
    }
}

/// "GPU · codec", either alone, or nothing when neither is known.
pub(crate) fn describe_gpu(name: Option<String>, codec: Option<Codec>) -> String {
    let codec = codec.map(crate::session::codec_label);
    match (name, codec) {
        (Some(name), Some(codec)) => format!("{name} · {codec}"),
        (Some(name), None) => name,
        (None, Some(codec)) => codec.into(),
        (None, None) => String::new(),
    }
}

/// The best codec (`CODEC_PREFERENCE`) the hardware behind `encoder`
/// encodes, as far as this build can ask.
#[allow(unused_variables)]
fn best_codec(encoder: &EncoderKind) -> Option<Codec> {
    crate::backends::CODEC_PREFERENCE
        .into_iter()
        .find(|&c| match encoder {
            #[cfg(feature = "vaapi")]
            EncoderKind::Vaapi { render_node } => fernsicht_codec::vaapi::encodes(render_node, c),
            #[cfg(feature = "nvidia")]
            EncoderKind::Nvenc { gpu } => fernsicht_codec::nvidia::encodes(*gpu, c),
            #[cfg(feature = "vulkan")]
            EncoderKind::Vulkan { render_node } => fernsicht_codec::vulkan::encodes(render_node, c),
            _ => false,
        })
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
    /// The monitor's scanout plane as DMA-BUF (needs the `kms` feature and
    /// CAP_SYS_ADMIN; VAAPI imports it, NVENC through Vulkan and CUDA, the
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
    /// Hardware AV1, HEVC or H.264 over VAAPI (needs the `vaapi` feature
    /// and a GPU).
    Vaapi { render_node: String },
    /// Hardware AV1, HEVC or H.264 with NVENC (needs the `nvidia` feature
    /// and an NVIDIA GPU; `gpu` is the CUDA device index).
    Nvenc { gpu: u32 },
    /// Hardware AV1, HEVC or H.264 with Vulkan Video on the GPU behind the
    /// render node (needs the `vulkan` feature and FFmpeg 7.1).
    Vulkan { render_node: String },
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
            clipboard: fernsicht_clipboard::Place::default(),
            security: None,
            control: None,
            description: HostDescription::default(),
            web: None,
            web_root: None,
            settings: Arc::default(),
            power_record: None,
            stun: vec![DEFAULT_STUN.into()],
            relay_max_kbps: 15_000,
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
    /// The encoder heals losses by intra refresh instead of keyframes (the
    /// client asked, and the encoder is NVENC).
    pub refresh: bool,
    /// The codecs of `codecs` the client decodes in 10 bit (D4).
    pub ten_bit: CodecSet,
    /// What the screen holds (its depth, HDR or not); the stream takes it
    /// in 10 bit where the client decodes that.
    pub screen: Colour,
    /// The client is reached through a TURN relay: smaller datagrams (the
    /// relay's path is not known) and a lower bitrate.
    pub relayed: bool,
}
