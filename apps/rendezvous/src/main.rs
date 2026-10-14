use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use fernsicht_rendezvous::{Config, serve, turn::TurnConfig};

/// Fernsicht's rendezvous service: hosts register here, clients find them
/// over the internet. Put it behind a reverse proxy for HTTPS.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Where to listen (plain HTTP; the proxy in front does TLS).
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: String,
    /// A TURN server to hand out credentials for, e.g.
    /// turn:turn.example.org:3478?transport=udp (repeatable). Needs
    /// FERNSICHT_TURN_SECRET: coturn's static-auth-secret.
    #[arg(long = "turn-url")]
    turn_urls: Vec<String>,
    /// How long handed out TURN credentials stay valid, seconds.
    #[arg(long, default_value_t = 4 * 3600)]
    turn_ttl: u64,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let turn = if args.turn_urls.is_empty() {
        None
    } else {
        let secret = std::env::var("FERNSICHT_TURN_SECRET")
            .context("--turn-url needs FERNSICHT_TURN_SECRET (coturn's static-auth-secret)")?;
        Some(TurnConfig {
            urls: args.turn_urls,
            secret,
            ttl: Duration::from_secs(args.turn_ttl),
        })
    };
    let listener =
        TcpListener::bind(&args.listen).with_context(|| format!("listen on {}", args.listen))?;
    log::info!(
        "rendezvous service on {} ({})",
        listener.local_addr()?,
        if turn.is_some() {
            "with TURN credentials"
        } else {
            "no relay"
        }
    );
    let stop = Arc::new(AtomicBool::new(false));
    fernsicht_core::signal::stop_on_signals(stop.clone())?;
    serve(listener, Config { turn }, stop)?
        .join()
        .map_err(|_| anyhow::anyhow!("the service thread panicked"))
}
