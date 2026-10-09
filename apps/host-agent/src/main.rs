use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use clap::Parser;
use fernsicht_host_agent::{HostAgent, HostConfig};

/// Fernsicht host agent (phase 1: test pattern + synthetic codec over UDP).
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// UDP address to listen on.
    #[arg(long, default_value = "0.0.0.0:47800")]
    bind: String,
    /// Upper bound for the resolution a client may request.
    #[arg(long, default_value_t = 1920)]
    max_width: u16,
    #[arg(long, default_value_t = 1080)]
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
    };
    let agent = HostAgent::bind(cfg)?;
    log::info!("listening on {}", agent.local_addr()?);
    agent.run(Arc::new(AtomicBool::new(false)))
}
