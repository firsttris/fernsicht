use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use clap::Parser;
use fernsicht_client::{ClientConfig, run};
use fernsicht_render::overlay::ms;

/// Fernsicht client (phase 1: headless, prints the latency overlay).
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Host agent address, e.g. 192.168.1.20:47800.
    host: String,
    #[arg(long, default_value_t = 1920)]
    width: u16,
    #[arg(long, default_value_t = 1080)]
    height: u16,
    #[arg(long, default_value_t = 60)]
    fps: u16,
    /// Requested bitrate in kbit/s.
    #[arg(long, default_value_t = 20_000)]
    bitrate: u32,
    /// Drop this fraction of incoming video packets (e.g. 0.01 = 1 %).
    #[arg(long, default_value_t = 0.0)]
    loss: f64,
    /// Stop after this many seconds (default: run until Ctrl+C).
    #[arg(long)]
    duration: Option<u64>,
    /// GPU render node for hardware decoding (VAAPI).
    #[arg(long, default_value = "/dev/dri/renderD128")]
    render_node: String,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let cfg = ClientConfig {
        host: args.host,
        width: args.width,
        height: args.height,
        fps: args.fps,
        bitrate_kbps: args.bitrate,
        loss: args.loss,
        duration: args.duration.map(Duration::from_secs),
        print_overlay: true,
        render_node: args.render_node,
        ..ClientConfig::default()
    };
    let s = run(cfg, Arc::new(AtomicBool::new(false)))?;
    println!();
    println!("── Zusammenfassung ──");
    println!(
        "Frames: {} angezeigt, {} komplett, {} verloren · Pakete: {} empfangen, {} verloren, {} per FEC repariert",
        s.frames_presented,
        s.receiver.frames_completed,
        s.receiver.frames_dropped,
        s.receiver.packets_received,
        s.receiver.packets_lost,
        s.receiver.packets_recovered
    );
    println!(
        "Glass-to-Glass (letzte {} Frames): Ø {} · p95 {} · max {}",
        s.total.samples,
        ms(s.total.avg),
        ms(s.total.p95),
        ms(s.total.max)
    );
    if s.decode_errors > 0 {
        println!("Decode-Fehler: {}", s.decode_errors);
    }
    if s.frames_skipped + s.frames_overflowed > 0 {
        println!(
            "Übersprungen (Anzeige hinterher): {} · verworfen (Decoder hinterher): {}",
            s.frames_skipped, s.frames_overflowed
        );
    }
    if s.frames_awaiting_keyframe > 0 {
        println!(
            "Verworfen bis zum ersten Keyframe: {}",
            s.frames_awaiting_keyframe
        );
    }
    Ok(())
}
