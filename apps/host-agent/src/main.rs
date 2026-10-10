use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clap::Parser;
use fernsicht_host_agent::{CaptureKind, EncoderKind, HostAgent, HostConfig, InputKind};

/// Fernsicht host agent (phase 1: test pattern or KMS capture, synthetic or
/// VAAPI H.264 over UDP).
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
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
    /// Video encoder: "synthetic" (no GPU), "vaapi" (AMD/Intel) or "nvenc"
    /// (NVIDIA); the hardware ones need the build features "vaapi"/"nvidia".
    #[arg(long, value_enum, default_value_t = EncoderArg::Synthetic)]
    encoder: EncoderArg,
    /// GPU render node for VAAPI.
    #[arg(long, default_value = "/dev/dri/renderD128")]
    render_node: String,
    /// Picture source: "test-pattern" or "kms" (the monitor; needs a build
    /// with the "kms" feature and CAP_SYS_ADMIN, see docs/kms-capture.md).
    #[arg(long, value_enum, default_value_t = CaptureArg::TestPattern)]
    capture: CaptureArg,
    /// Accept mouse and keyboard from the client (virtual devices through
    /// /dev/uinput). There is no authentication yet: anyone who reaches the
    /// port can then type on this machine. Only in a trusted LAN.
    #[arg(long)]
    input: bool,
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

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CaptureArg {
    TestPattern,
    Kms,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum EncoderArg {
    Synthetic,
    Vaapi,
    Nvenc,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let cfg = HostConfig {
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
        input: if args.input {
            log::warn!("input enabled without authentication: use only in a trusted LAN");
            InputKind::Uinput
        } else {
            InputKind::Off
        },
        encoder: match args.encoder {
            EncoderArg::Synthetic => EncoderKind::Synthetic,
            EncoderArg::Vaapi => EncoderKind::Vaapi {
                render_node: args.render_node,
            },
            EncoderArg::Nvenc => EncoderKind::Nvenc {
                gpu: args.cuda_device,
            },
        },
        ..HostConfig::default()
    };
    let agent = HostAgent::bind(cfg)?;
    log::info!("listening on {}", agent.local_addr()?);
    agent.run(Arc::new(AtomicBool::new(false)))
}
