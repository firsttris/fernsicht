use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use clap::Parser;
use fernsicht_client::discover::{broadcast_targets, discover};
use fernsicht_client::{
    AppLine, AudioOutput, ClientConfig, ClientSecurity, CodecChoice, DEFAULT_PORT, DecoderChoice,
    Failure, FailureCode, Identity, OverlayOutput, Peer, RunSummary, Trusted, default_state_dir,
    failure_of, find_host, pair, printable, remember_address, run, with_default_port,
};
use fernsicht_render::overlay::ms;

/// Fernsicht client: shows the host's screen in a window (build feature
/// "window"; Ctrl+Alt+Shift+Q closes, Ctrl+Alt+Shift+F toggles fullscreen,
/// Esc and F11 with --view-only) and prints the latency overlay.
#[derive(Parser, Debug)]
#[command(
    version,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// The paired host: its name or address, e.g. zentrale or
    /// 192.168.1.20 (port 47800 unless given).
    #[arg(required = true)]
    host: Option<String>,
    /// The paired host's key (fingerprint): picks it for sure, and HOST is
    /// then where to reach it (the app passes both).
    #[arg(long)]
    key: Option<String>,
    /// Where this device's key and paired hosts are kept (default:
    /// ~/.config/fernsicht).
    #[arg(long, global = true)]
    state_dir: Option<std::path::PathBuf>,
    /// Stream size; 0 (default) = the host's screen size.
    #[arg(long, default_value_t = 0)]
    width: u16,
    /// Stream height; 0 (default) = the host's screen size.
    #[arg(long, default_value_t = 0)]
    height: u16,
    /// Frames per second (no more than the host's monitor shows).
    #[arg(long, default_value_t = 60)]
    fps: u16,
    /// Bitrate in kbit/s; 0 (default) = chosen by the host for the size
    /// (20 Mbit/s for 1080p60, about 36 for 1440p60).
    #[arg(long, default_value_t = 0)]
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
    /// Save the received video to this file (`ffplay file.h264`; HEVC as
    /// `file.h265`; AV1 is raw OBUs, `ffplay -f obu file.obu`).
    #[arg(long)]
    record: Option<std::path::PathBuf>,
    /// Hardware decoder: "auto" (Vulkan Video, else VAAPI, else NVDEC),
    /// "vulkan", "vaapi" or "nvdec".
    #[arg(long, value_enum, default_value_t = DecoderArg::Auto)]
    decoder: DecoderArg,
    /// Video codec: "auto" (the best this machine decodes in hardware and
    /// the host encodes: AV1, HEVC, else H.264), "h264", "hevc" or "av1".
    #[arg(long, value_enum, default_value_t = CodecArg::Auto)]
    codec: CodecArg,
    /// Do not play the host's sound.
    #[arg(long)]
    no_audio: bool,
    /// Only watch: send no mouse or keyboard input (the host also needs
    /// --input to accept it).
    #[arg(long)]
    view_only: bool,
    /// No window: decode only and print the overlay.
    #[arg(long)]
    headless: bool,
    /// Run for the desktop app: the overlay as JSON lines on stdout (and a
    /// failure as the last one), commands on stdin ("gaming", "desktop",
    /// "mute", "unmute", "keys 29,56,111", "monitor 1|next|previous"), and
    /// stop when stdin closes (the app quit or ended the session).
    #[arg(long, hide = true)]
    app: bool,
    /// Gaming mode: a click into the window captures the pointer, which
    /// then moves relatively (as games want it). Ctrl+Alt+Shift+M captures
    /// and lets go in either mode.
    #[arg(long)]
    gaming: bool,
    /// Leave this machine's gamepads out.
    #[arg(long)]
    no_gamepad: bool,
    /// After a packet loss keep the picture going and let it heal (intra
    /// refresh, NVIDIA hosts) instead of waiting for a fresh keyframe:
    /// smooth instead of sharp, and no bitrate bursts. Good on Wi-Fi.
    #[arg(long)]
    smooth: bool,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Pair with a host: run "fernsicht-host-agent pair" there (or start it
    /// with --pair), then enter the PIN it shows.
    Pair {
        /// The host's address (e.g. 192.168.1.20) or its name as
        /// "fernsicht-client discover" lists it.
        host: String,
        /// The PIN the host shows.
        pin: String,
        /// How the host will list this device (default: its hostname).
        #[arg(long)]
        name: Option<String>,
    },
    /// List the paired hosts.
    Hosts,
    /// Look for hosts in the local network.
    Discover {
        /// Ask these addresses too, e.g. a host in another network or a
        /// network's broadcast address (192.168.1.255).
        targets: Vec<String>,
        /// How long to wait for answers, seconds.
        #[arg(long, default_value_t = 1.5)]
        wait: f64,
    },
    /// Forget a paired host.
    Forget {
        /// Its name or address.
        host: String,
    },
}

fn hostname() -> String {
    fernsicht_core::sysinfo::hostname().unwrap_or_else(|| "client".into())
}

/// Where to pair: an address as given, else the host of that name in the
/// LAN, else the name for DNS.
fn pairing_address(host: &str) -> String {
    let addr = with_default_port(host);
    if addr.parse::<SocketAddr>().is_ok() {
        return addr;
    }
    match discover(&broadcast_targets(DEFAULT_PORT), Duration::from_secs(1)) {
        Ok(found) => found
            .iter()
            .find(|f| f.name == host)
            .map_or(addr, |f| f.addr.to_string()),
        Err(_) => addr,
    }
}

/// Pairing, listing, forgetting: done, nothing to stream.
fn manage(command: Command, dir: &std::path::Path) -> anyhow::Result<()> {
    let hosts_path = dir.join("hosts.json");
    let mut hosts = Trusted::load(&hosts_path).map_err(anyhow::Error::msg)?;
    match command {
        Command::Pair { host, pin, name } => {
            let identity =
                Identity::load_or_create(&dir.join("client.json")).map_err(anyhow::Error::msg)?;
            let addr = pairing_address(&host);
            let peer = pair(&addr, &pin, &identity, &name.unwrap_or_else(hostname))?;
            println!(
                "Paired with {} ({addr}, key {}).",
                peer.name,
                peer.key.fingerprint()
            );
            println!("To connect: fernsicht-client {}", peer.name);
            hosts.add(peer);
            hosts.save(&hosts_path).map_err(anyhow::Error::msg)?;
        }
        Command::Hosts => {
            if hosts.peers.is_empty() {
                println!("No hosts paired yet (fernsicht-client pair HOST PIN).");
            }
            for p in &hosts.peers {
                println!(
                    "{}  {}  key {}",
                    p.name,
                    p.address.as_deref().unwrap_or("?"),
                    p.key.fingerprint()
                );
            }
        }
        Command::Discover {
            targets: extra,
            wait,
        } => {
            let mut targets = broadcast_targets(DEFAULT_PORT);
            for t in &extra {
                let addr = with_default_port(t);
                targets.push(
                    addr.parse()
                        .map_err(|_| anyhow::anyhow!("not an address: {t}"))?,
                );
            }
            // Paired hosts directly, too: that also works where broadcasts
            // do not get through.
            targets.extend(
                hosts
                    .peers
                    .iter()
                    .filter_map(|p| p.address.as_deref()?.parse::<SocketAddr>().ok()),
            );
            let found = discover(&targets, Duration::from_secs_f64(wait.clamp(0.1, 30.0)))?;
            if found.is_empty() {
                println!("No hosts found.");
            }
            for f in &found {
                let mut notes = Vec::new();
                if hosts.get(&f.key).is_some() {
                    notes.push("paired");
                }
                if f.pairing {
                    notes.push("pairing open");
                }
                if f.busy {
                    notes.push("in use");
                }
                let about: Vec<&str> = [f.os.as_str(), f.gpu.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect();
                // Name, OS and GPU are what anyone on the LAN answered:
                // no control characters reach the terminal.
                println!(
                    "{}  {}  {}  {}",
                    printable(&f.name),
                    f.addr,
                    printable(&about.join(" · ")),
                    notes.join(", ")
                );
            }
        }
        Command::Forget { host } => {
            let key = find_host(&hosts, &host)
                .map(|p| p.key)
                .ok_or_else(|| anyhow::anyhow!("no paired host {host}"))?;
            hosts.remove(&key);
            hosts.save(&hosts_path).map_err(anyhow::Error::msg)?;
            println!("{host} forgotten.");
        }
    }
    Ok(())
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum DecoderArg {
    Auto,
    Vulkan,
    Vaapi,
    Nvdec,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CodecArg {
    Auto,
    H264,
    Hevc,
    Av1,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut args = Args::parse();
    let dir = args.state_dir.clone().unwrap_or_else(default_state_dir);
    if let Some(command) = args.command.take() {
        return manage(command, &dir);
    }
    let app = args.app;
    let result = stream(args, &dir);
    // The app tells failures apart by the last line's code.
    if app
        && let Err(e) = &result
        && let Ok(line) = serde_json::to_string(&AppLine::Failed {
            failure: failure_of(e),
        })
    {
        println!("{line}");
    }
    result
}

/// Streams from the host `args` name until the session ends.
fn stream(args: Args, dir: &std::path::Path) -> anyhow::Result<()> {
    let wanted = args.host.clone().expect("required by clap");
    let hosts = Trusted::load(&dir.join("hosts.json")).map_err(anyhow::Error::msg)?;
    let found = match &args.key {
        Some(key) => hosts.peers.iter().find(|p| p.key.fingerprint() == *key),
        None => find_host(&hosts, &wanted),
    };
    let Some(host) = found.cloned() else {
        return Err(Failure::new(
            FailureCode::NotPaired,
            format!(
                "not paired with {wanted}. On the host run \"fernsicht-host-agent pair\", \
                 then here: fernsicht-client pair {wanted} PIN"
            ),
        )
        .into());
    };
    let identity =
        Identity::load_or_create(&dir.join("client.json")).map_err(anyhow::Error::msg)?;
    let cfg = session_config(&args, &wanted, &host, identity);
    let stop = Arc::new(AtomicBool::new(false));
    // Before any thread starts: Ctrl+C (or SIGTERM) ends the session with a
    // goodbye, so nothing stays pressed on the host.
    fernsicht_core::signal::stop_on_signals(stop.clone())?;
    let (commands_tx, commands) = crossbeam_channel::unbounded();
    if args.app {
        read_app_commands(stop.clone(), &cfg, commands_tx);
    } else {
        drop(commands_tx);
    }
    #[cfg(not(feature = "window"))]
    let _ = commands;
    let target = cfg.host.clone();
    let s = if args.headless || !cfg!(feature = "window") {
        if !args.headless {
            log::info!("built without the \"window\" feature: running headless");
        }
        run(cfg, stop)?
    } else {
        #[cfg(feature = "window")]
        {
            window::run(
                cfg,
                window::Options {
                    send_input: !args.view_only,
                    gaming: args.gaming,
                    gamepads: !args.view_only && !args.no_gamepad,
                    commands,
                },
                stop,
            )?
        }
        #[cfg(not(feature = "window"))]
        unreachable!()
    };
    // The handshake went through at `target`: it is the host's address now
    // (a new one from DHCP, say). Read again: the app may have changed the
    // list meanwhile.
    if s.session_id.is_some() {
        let path = dir.join("hosts.json");
        if let Ok(now) = Trusted::load(&path)
            && let Some(next) = remember_address(&now, &host.key, &target)
            && let Err(e) = next.save(&path)
        {
            log::warn!("remembering the host's address: {e}");
        }
    }
    print_summary(&s);
    Ok(())
}

/// The session `args` ask for with the paired `host`.
fn session_config(args: &Args, wanted: &str, host: &Peer, identity: Identity) -> ClientConfig {
    // An address given (by the app: where discovery just found the host)
    // is used; for a name, where the host was last.
    let target = if args.key.is_some() || fernsicht_client::is_address(wanted) {
        with_default_port(wanted)
    } else {
        host.address
            .clone()
            .unwrap_or_else(|| with_default_port(wanted))
    };
    ClientConfig {
        host: target,
        security: Some(Arc::new(ClientSecurity {
            identity,
            host: host.key,
        })),
        width: args.width,
        height: args.height,
        fps: args.fps,
        bitrate_kbps: args.bitrate,
        loss: args.loss,
        duration: args.duration.map(Duration::from_secs),
        print_overlay: if args.app {
            OverlayOutput::Json
        } else {
            OverlayOutput::Text
        },
        render_node: args.render_node.clone(),
        decoder: match args.decoder {
            DecoderArg::Auto => DecoderChoice::Auto,
            DecoderArg::Vulkan => DecoderChoice::Vulkan,
            DecoderArg::Vaapi => DecoderChoice::Vaapi,
            DecoderArg::Nvdec => DecoderChoice::Nvdec,
        },
        codec: match args.codec {
            CodecArg::Auto => CodecChoice::Auto,
            CodecArg::H264 => CodecChoice::H264,
            CodecArg::Hevc => CodecChoice::Hevc,
            CodecArg::Av1 => CodecChoice::Av1,
        },
        record: args.record.clone(),
        refresh: args.smooth,
        audio: if args.no_audio {
            AudioOutput::Off
        } else {
            AudioOutput::Speakers
        },
        ..ClientConfig::default()
    }
}

/// Reads the app's commands on stdin until it lets go, which ends the
/// session.
fn read_app_commands(
    stop: Arc<AtomicBool>,
    cfg: &ClientConfig,
    commands: crossbeam_channel::Sender<AppCommand>,
) {
    let muted = cfg.muted.clone();
    let monitors = cfg.monitors.clone();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            match line.trim() {
                "mute" => muted.store(true, std::sync::atomic::Ordering::Relaxed),
                "unmute" => muted.store(false, std::sync::atomic::Ordering::Relaxed),
                "gaming" => {
                    let _ = commands.send(AppCommand::Mode(true));
                }
                "desktop" => {
                    let _ = commands.send(AppCommand::Mode(false));
                }
                line => {
                    if let Some(codes) = fernsicht_client::parse_keys_command(line) {
                        let _ = commands.send(AppCommand::Keys(codes));
                    } else if let Some(r) = fernsicht_client::parse_monitor_command(line) {
                        monitors.request(r);
                    } else {
                        log::debug!("unknown command {line:?}");
                    }
                }
            }
        }
        // The app let go of stdin: the session is over.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

fn print_summary(s: &RunSummary) {
    println!();
    println!("── Summary ──");
    if let Some(decoder) = s.decoder {
        println!("Decoded with {decoder}");
    }
    println!(
        "Frames: {} shown, {} complete, {} lost · packets: {} received, {} lost, {} repaired by FEC",
        s.frames_presented,
        s.receiver.frames_completed,
        s.receiver.frames_dropped,
        s.receiver.packets_received,
        s.receiver.packets_lost,
        s.receiver.packets_recovered
    );
    println!(
        "Glass-to-glass (last {} frames): avg {} · p95 {} · max {}",
        s.total.samples,
        ms(s.total.avg),
        ms(s.total.p95),
        ms(s.total.max)
    );
    println!(
        "Pointer: {} positions, {} images received",
        s.cursor_positions, s.cursor_shapes
    );
    if s.audio.played > 0 {
        println!(
            "Sound: {:.1} s played, delay avg {} · {} frames concealed, {} dropped",
            s.audio.played as f64 * f64::from(fernsicht_audio::FRAME_MS) / 1000.0,
            ms(s.audio.delay_us as u32),
            s.audio.concealed,
            s.audio.dropped
        );
    }
    if s.decode_errors > 0 {
        println!("Decode errors: {}", s.decode_errors);
    }
    if s.frames_skipped + s.frames_overflowed > 0 {
        println!(
            "Skipped (display behind): {} · dropped (decoder behind): {}",
            s.frames_skipped, s.frames_overflowed
        );
    }
    if s.frames_awaiting_keyframe > 0 {
        println!(
            "Dropped until the first keyframe: {}",
            s.frames_awaiting_keyframe
        );
    }
}

/// What the app tells a running client on stdin (used by the window).
#[cfg_attr(not(feature = "window"), allow(dead_code))]
enum AppCommand {
    /// Gaming (true) or desktop mode.
    Mode(bool),
    /// A key combination from the "send keys" menu.
    Keys(Vec<u16>),
}

#[cfg(feature = "window")]
mod shortcuts;

#[cfg(feature = "window")]
mod window;
