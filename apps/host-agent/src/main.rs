use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clap::Parser;
use fernsicht_host_agent::{
    AudioKind, CaptureKind, EncoderKind, HostAgent, HostConfig, HostDescription, HostSecurity,
    InputKind, PAIRING_OPEN_FOR,
};

/// Fernsicht host: streams this computer's screen to paired clients.
/// Without a command it runs the host; the commands talk to a running host
/// (e.g. the system service).
#[derive(Parser, Debug)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// The running host's control socket (default: /run/fernsicht/control.sock
    /// for the service, else in the user's runtime directory).
    #[arg(long, global = true)]
    control: Option<std::path::PathBuf>,
    /// UDP address to listen on.
    #[arg(long, default_value = "0.0.0.0:47800")]
    bind: String,
    /// Upper bound for the stream resolution. A client asking for the
    /// host's resolution gets the screen's, scaled down to fit this.
    #[arg(long, default_value_t = 3840)]
    max_width: u16,
    #[arg(long, default_value_t = 2160)]
    max_height: u16,
    /// Upper bound for the frame rate a client may request.
    #[arg(long, default_value_t = 144)]
    max_fps: u16,
    /// Upper bound for the bitrate a client may request, kbit/s.
    #[arg(long, default_value_t = 80_000)]
    max_bitrate: u32,
    /// Drop this fraction of outgoing video packets (e.g. 0.01 = 1 %).
    #[arg(long, default_value_t = 0.0)]
    loss: f64,
    /// Pacing rate in Mbit/s.
    #[arg(long, default_value_t = 400)]
    pace_mbit: u64,
    /// Video encoder: "auto" (the GPU's: VAAPI on AMD/Intel, NVENC on
    /// NVIDIA), "vaapi", "nvenc" or "synthetic" (no GPU, no real picture);
    /// the hardware ones need the build features "vaapi"/"nvidia".
    #[arg(long, value_enum, default_value_t = EncoderArg::Synthetic)]
    encoder: EncoderArg,
    /// GPU render node for --encoder vaapi.
    #[arg(long, default_value = "/dev/dri/renderD128")]
    render_node: String,
    /// Picture source: "test-pattern" or "kms" (the monitor; needs a build
    /// with the "kms" feature and CAP_SYS_ADMIN, see docs/kms-capture.md).
    #[arg(long, value_enum, default_value_t = CaptureArg::TestPattern)]
    capture: CaptureArg,
    /// Sound to send: "desktop" (what this computer plays), "tone" (a
    /// 440 Hz test tone) or "off". Default: desktop with --capture kms,
    /// off with the test pattern.
    #[arg(long, value_enum)]
    audio: Option<AudioArg>,
    /// Open pairing for 5 minutes: shows a PIN to enter on the new device
    /// (fernsicht-client pair HOST PIN).
    #[arg(long)]
    pair: bool,
    /// Where the host's key and paired clients are kept (default:
    /// /var/lib/fernsicht as root, else ~/.config/fernsicht/host).
    #[arg(long)]
    state_dir: Option<std::path::PathBuf>,
    /// Accept mouse and keyboard from paired clients (virtual devices
    /// through /dev/uinput).
    #[arg(long)]
    input: bool,
    /// TCP address of the web viewer (page and API for browsers).
    #[arg(long, default_value = "0.0.0.0:47800")]
    web: String,
    /// No web viewer.
    #[arg(long)]
    no_web: bool,
    /// Leave the GPU's clocks alone during sessions (by default they are
    /// raised while someone watches, see docs/install.md; also switchable
    /// in the app).
    #[arg(long)]
    no_gpu_boost: bool,
    /// The built web viewer (web/viewer/dist). Default:
    /// ../share/fernsicht/viewer next to this program, if it exists.
    #[arg(long)]
    web_root: Option<std::path::PathBuf>,
    /// NVENC: CUDA device index of the NVIDIA GPU.
    #[arg(long, default_value_t = 0)]
    cuda_device: u32,
    /// KMS: card node, e.g. /dev/dri/card1 (default: first with a display).
    #[arg(long)]
    kms_card: Option<String>,
    /// KMS: connector, e.g. DP-1 (default: first active display).
    #[arg(long)]
    kms_connector: Option<String>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Open pairing on the running host and show the PIN to enter on the
    /// new device (fernsicht-client pair HOST PIN).
    Pair,
    /// Show the running host: its key, paired devices, the session.
    Status,
    /// Forget a paired device (its name or key).
    Unpair { device: String },
}

/// Commands for a running host.
fn command(cmd: Command, control: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    use fernsicht_host_agent::control::request;
    use serde_json::json;
    let path = control.unwrap_or_else(fernsicht_host_agent::control::running_host_path);
    match cmd {
        Command::Pair => {
            let r = request(&path, &json!({"cmd": "pair"}))?;
            println!(
                "Kopplung offen für {} Minuten. PIN: {}",
                r["expires_in_s"].as_u64().unwrap_or(300) / 60,
                r["pin"].as_str().unwrap_or("?")
            );
            println!("Auf dem neuen Gerät: fernsicht-client pair <diese Adresse> <PIN>");
        }
        Command::Status => {
            let r = request(&path, &json!({"cmd": "status"}))?;
            println!(
                "Host {} (Schlüssel {})",
                r["name"].as_str().unwrap_or("?"),
                r["key"].as_str().unwrap_or("?")
            );
            match r["session"].as_object() {
                Some(s) => println!(
                    "Verbunden: {} ({}), {}×{} bei {} fps{}",
                    s["client"].as_str().unwrap_or("?"),
                    s["address"].as_str().unwrap_or("?"),
                    s["width"],
                    s["height"],
                    s["fps"],
                    if s["encrypted"] == true {
                        ", verschlüsselt"
                    } else {
                        ""
                    }
                ),
                None => println!("Keine Verbindung."),
            }
            if let Some(secs) = r["pairing"].as_u64() {
                println!("Kopplung offen, noch {secs} s.");
            }
            let paired = r["paired"].as_array().cloned().unwrap_or_default();
            println!("Gekoppelte Geräte: {}", paired.len());
            for p in paired {
                println!(
                    "  {}  {}",
                    p["name"].as_str().unwrap_or("?"),
                    p["key"].as_str().unwrap_or("?")
                );
            }
        }
        Command::Unpair { device } => {
            request(&path, &json!({"cmd": "unpair", "device": device}))?;
            println!("{device} entfernt.");
        }
    }
    Ok(())
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum AudioArg {
    Desktop,
    Tone,
    Off,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CaptureArg {
    TestPattern,
    Kms,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum EncoderArg {
    Auto,
    Synthetic,
    Vaapi,
    Nvenc,
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "host".into())
}

/// Root (a service, or sudo for KMS capture) keeps its key in /var/lib;
/// a user in ~/.config.
fn default_state_dir() -> std::path::PathBuf {
    if std::fs::metadata("/proc/self").is_ok_and(|m| {
        use std::os::unix::fs::MetadataExt;
        m.uid() == 0
    }) {
        return "/var/lib/fernsicht".into();
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| ".".into());
    base.join("fernsicht").join("host")
}

/// The viewer installed next to this program (packaging/install-host.sh).
fn installed_viewer() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("share/fernsicht/viewer");
    dir.join("index.html").is_file().then_some(dir)
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    if let Some(cmd) = args.command {
        return command(cmd, args.control);
    }
    let encoder = match args.encoder {
        EncoderArg::Auto => fernsicht_host_agent::auto_encoder()?,
        EncoderArg::Synthetic => EncoderKind::Synthetic,
        EncoderArg::Vaapi => EncoderKind::Vaapi {
            render_node: args.render_node,
        },
        EncoderArg::Nvenc => EncoderKind::Nvenc {
            gpu: args.cuda_device,
        },
    };
    let cfg = HostConfig {
        control: Some(
            args.control
                .unwrap_or_else(fernsicht_host_agent::control::default_path),
        ),
        bind: args.bind,
        max_width: args.max_width,
        max_height: args.max_height,
        max_fps: args.max_fps,
        max_bitrate_kbps: args.max_bitrate,
        loss: args.loss,
        pace_bytes_per_sec: args.pace_mbit * 1_000_000 / 8,
        capture: match args.capture {
            CaptureArg::TestPattern => CaptureKind::TestPattern,
            CaptureArg::Kms => CaptureKind::Kms {
                card: args.kms_card,
                connector: args.kms_connector,
            },
        },
        audio: match (args.audio, args.capture) {
            (Some(AudioArg::Desktop), _) | (None, CaptureArg::Kms) => AudioKind::Desktop,
            (Some(AudioArg::Tone), _) => AudioKind::Tone,
            (Some(AudioArg::Off), _) | (None, CaptureArg::TestPattern) => AudioKind::Off,
        },
        input: if args.input {
            InputKind::Uinput
        } else {
            InputKind::Off
        },
        description: HostDescription::of_this_machine(&encoder),
        web: (!args.no_web).then(|| args.web.clone()),
        web_root: args.web_root.clone().or_else(installed_viewer),
        encoder,
        ..HostConfig::default()
    };
    let dir = args.state_dir.clone().unwrap_or_else(default_state_dir);
    let security = HostSecurity::load(&dir, &hostname())?;
    log::info!(
        "host key {} ({}), {} paired client(s)",
        security.public_key().fingerprint(),
        dir.display(),
        security.paired().len()
    );
    if args.pair {
        let pin = fernsicht_secure::pairing::new_pin();
        security.open_pairing(&pin);
        println!(
            "Kopplung offen für {} Minuten. PIN: {pin}",
            PAIRING_OPEN_FOR.as_secs() / 60
        );
    }
    let mut settings = fernsicht_host_agent::HostSettings::load(dir.join("settings.json"));
    if args.no_gpu_boost {
        settings = settings.without_gpu_boost();
    }
    // SAFETY: geteuid has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    let cfg = HostConfig {
        security: Some(std::sync::Arc::new(security)),
        settings: Arc::new(settings),
        power_record: root.then(|| "/run/fernsicht/gpu-power.json".into()),
        ..cfg
    };
    let agent = HostAgent::bind(cfg)?;
    log::info!("listening on {}", agent.local_addr()?);
    if let Some(web) = agent.web_addr() {
        log::info!("web viewer on http://{web}");
    }
    agent.run(Arc::new(AtomicBool::new(false)))
}
