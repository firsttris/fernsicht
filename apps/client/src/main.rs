use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use clap::Parser;
use fernsicht_client::discover::{broadcast_targets, discover};
use fernsicht_client::{
    AudioOutput, ClientConfig, ClientSecurity, CodecChoice, DEFAULT_PORT, DecoderChoice, Identity,
    OverlayOutput, Trusted, default_state_dir, find_host, pair, refresh_addresses, run,
    with_default_port,
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
    /// Where this device's key and paired hosts are kept (default:
    /// ~/.config/fernsicht).
    #[arg(long, global = true)]
    state_dir: Option<std::path::PathBuf>,
    /// Stream size; 0 (default) = the host's screen size.
    #[arg(long, default_value_t = 0)]
    width: u16,
    #[arg(long, default_value_t = 0)]
    height: u16,
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
    /// Hardware decoder: "auto" (VAAPI, else NVDEC), "vaapi" or "nvdec".
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
    /// Run for the desktop app: the overlay as JSON lines on stdout,
    /// commands on stdin ("gaming", "desktop", "mute", "unmute"), and stop
    /// when stdin closes (the app quit or ended the session).
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
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "client".into())
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
                "Gekoppelt mit {} ({addr}, Schlüssel {}).",
                peer.name,
                peer.key.fingerprint()
            );
            println!("Verbinden: fernsicht-client {}", peer.name);
            hosts.add(peer);
            hosts.save(&hosts_path).map_err(anyhow::Error::msg)?;
        }
        Command::Hosts => {
            if hosts.peers.is_empty() {
                println!("Noch keine Hosts gekoppelt (fernsicht-client pair HOST PIN).");
            }
            for p in &hosts.peers {
                println!(
                    "{}  {}  Schlüssel {}",
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
                println!("Keine Hosts gefunden.");
            }
            for f in &found {
                let mut notes = Vec::new();
                if hosts.get(&f.key).is_some() {
                    notes.push("gekoppelt");
                }
                if f.pairing {
                    notes.push("Kopplung offen");
                }
                if f.busy {
                    notes.push("verbunden");
                }
                let about: Vec<&str> = [f.os.as_str(), f.gpu.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect();
                println!(
                    "{}  {}  {}  {}",
                    f.name,
                    f.addr,
                    about.join(" · "),
                    notes.join(", ")
                );
            }
            if refresh_addresses(&mut hosts, &found) {
                hosts.save(&hosts_path).map_err(anyhow::Error::msg)?;
            }
        }
        Command::Forget { host } => {
            let key = find_host(&hosts, &host)
                .map(|p| p.key)
                .ok_or_else(|| anyhow::anyhow!("no paired host {host}"))?;
            hosts.remove(&key);
            hosts.save(&hosts_path).map_err(anyhow::Error::msg)?;
            println!("{host} vergessen.");
        }
    }
    Ok(())
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum DecoderArg {
    Auto,
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
    let args = Args::parse();
    let dir = args.state_dir.clone().unwrap_or_else(default_state_dir);
    if let Some(command) = args.command {
        return manage(command, &dir);
    }
    let wanted = args.host.clone().expect("required by clap");
    let hosts = Trusted::load(&dir.join("hosts.json")).map_err(anyhow::Error::msg)?;
    let Some(host) = find_host(&hosts, &wanted).cloned() else {
        anyhow::bail!(
            "not paired with {wanted}. On the host run \"fernsicht-host-agent pair\", \
             then here: fernsicht-client pair {wanted} PIN"
        );
    };
    let identity =
        Identity::load_or_create(&dir.join("client.json")).map_err(anyhow::Error::msg)?;
    let cfg = ClientConfig {
        host: host
            .address
            .clone()
            .unwrap_or_else(|| with_default_port(&wanted)),
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
        render_node: args.render_node,
        decoder: match args.decoder {
            DecoderArg::Auto => DecoderChoice::Auto,
            DecoderArg::Vaapi => DecoderChoice::Vaapi,
            DecoderArg::Nvdec => DecoderChoice::Nvdec,
        },
        codec: match args.codec {
            CodecArg::Auto => CodecChoice::Auto,
            CodecArg::H264 => CodecChoice::H264,
            CodecArg::Hevc => CodecChoice::Hevc,
            CodecArg::Av1 => CodecChoice::Av1,
        },
        record: args.record,
        audio: if args.no_audio {
            AudioOutput::Off
        } else {
            AudioOutput::Speakers
        },
        ..ClientConfig::default()
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (commands_tx, commands) = crossbeam_channel::unbounded();
    if args.app {
        let (stop, muted) = (stop.clone(), cfg.muted.clone());
        let monitors = cfg.monitors.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                match line.trim() {
                    "mute" => muted.store(true, std::sync::atomic::Ordering::Relaxed),
                    "unmute" => muted.store(false, std::sync::atomic::Ordering::Relaxed),
                    "gaming" => {
                        let _ = commands_tx.send(AppCommand::Mode(true));
                    }
                    "desktop" => {
                        let _ = commands_tx.send(AppCommand::Mode(false));
                    }
                    line => {
                        if let Some(codes) = fernsicht_client::parse_keys_command(line) {
                            let _ = commands_tx.send(AppCommand::Keys(codes));
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
    } else {
        drop(commands_tx);
    }
    #[cfg(not(feature = "window"))]
    let _ = commands;
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
    println!(
        "Mauszeiger: {} Positionen, {} Bilder empfangen",
        s.cursor_positions, s.cursor_shapes
    );
    if s.audio.played > 0 {
        println!(
            "Ton: {:.1} s gespielt, Verzögerung Ø {} · {} Frames überbrückt, {} verworfen",
            s.audio.played as f64 * 0.005,
            ms(s.audio.delay_us as u32),
            s.audio.concealed,
            s.audio.dropped
        );
    }
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
mod window {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    use fernsicht_client::{
        ClientConfig, InputEvent, InputHandle, PresenterFactory, RunSummary, buttons, run_with,
    };
    use fernsicht_render::Presenter;
    use fernsicht_render::vulkan::window::WindowPresenter;
    use winit::application::ApplicationHandler;
    use winit::dpi::LogicalSize;
    use winit::event::{
        DeviceEvent, DeviceId, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent,
    };
    use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
    use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
    use winit::platform::scancode::PhysicalKeyExtScancode;
    use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

    enum UserEvent {
        /// The client thread has ended (duration over, host gone, error).
        Done,
        /// The app switched between gaming (true) and desktop mode.
        Gaming(bool),
        /// The app's "send keys" menu: a combination for the host.
        Keys(Vec<u16>),
    }

    /// How the window behaves.
    pub struct Options {
        /// Mouse, keyboard and gamepads go to the host (not view-only).
        pub send_input: bool,
        /// A click captures the pointer (relative motion for games).
        pub gaming: bool,
        /// This machine's gamepads go to the host.
        pub gamepads: bool,
        /// Mode switches and key combinations from the app.
        pub commands: crossbeam_channel::Receiver<crate::AppCommand>,
    }

    /// Wheel units (120 per notch) per pixel of touchpad scrolling.
    const WHEEL_PER_PIXEL: f64 = 8.0;

    struct App {
        cfg: Option<ClientConfig>,
        stop: Arc<AtomicBool>,
        proxy: EventLoopProxy<UserEvent>,
        window: Option<Arc<Window>>,
        client: Option<JoinHandle<anyhow::Result<RunSummary>>>,
        error: Option<anyhow::Error>,
        /// Mouse and keyboard go to the host (not view-only).
        input: Option<Arc<InputHandle>>,
        modifiers: ModifiersState,
        /// Fractions of wheel units from touchpads, carried over.
        scroll_rest: (f64, f64),
        gaming: bool,
        /// The pointer is captured: motion goes to the host relatively.
        captured: bool,
        /// Fractions of pixels of relative motion, carried over.
        motion_rest: (f64, f64),
        focused: bool,
        /// Passes the desktop's shortcuts (Meta, Alt+Tab) to the host.
        shortcuts: Option<crate::shortcuts::ShortcutLock>,
        /// The host's monitors; Ctrl+Alt+Shift+←/→ switches.
        monitors: Arc<fernsicht_client::MonitorControl>,
    }

    fn mouse_button(b: MouseButton) -> Option<u16> {
        Some(match b {
            MouseButton::Left => buttons::LEFT,
            MouseButton::Right => buttons::RIGHT,
            MouseButton::Middle => buttons::MIDDLE,
            MouseButton::Back => buttons::SIDE,
            MouseButton::Forward => buttons::EXTRA,
            MouseButton::Other(_) => return None,
        })
    }

    impl App {
        fn close(&mut self, event_loop: &ActiveEventLoop) {
            if let Some(input) = &self.input {
                input.release_all();
            }
            self.stop.store(true, Ordering::Relaxed);
            event_loop.exit();
        }

        /// Captures the pointer (locked in place, relative motion) or lets
        /// it go.
        fn capture(&mut self, on: bool) {
            let Some(w) = &self.window else { return };
            if self.input.is_none() || on == self.captured {
                return;
            }
            if on {
                let grabbed = w
                    .set_cursor_grab(CursorGrabMode::Locked)
                    .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined));
                if let Err(e) = grabbed {
                    log::warn!("cannot capture the pointer: {e}");
                    return;
                }
                log::info!("pointer captured; Ctrl+Alt+Shift+M lets go");
            } else {
                let _ = w.set_cursor_grab(CursorGrabMode::None);
            }
            self.captured = on;
            self.motion_rest = (0.0, 0.0);
            self.update_shortcuts();
        }

        /// Hands the desktop's shortcuts to the host or takes them back,
        /// per [`crate::shortcuts::wanted`].
        fn update_shortcuts(&mut self) {
            let fullscreen = self
                .window
                .as_ref()
                .is_some_and(|w| w.fullscreen().is_some());
            let on = crate::shortcuts::wanted(
                self.focused,
                self.input.is_some(),
                fullscreen,
                self.captured,
            );
            if let Some(lock) = &mut self.shortcuts {
                lock.set(on);
            }
        }

        fn toggle_fullscreen(&self) {
            if let Some(w) = &self.window {
                let full = w.fullscreen().is_some();
                w.set_fullscreen((!full).then_some(Fullscreen::Borderless(None)));
            }
        }

        /// Client commands. With input going to the host, Esc and F11
        /// belong to the host, so the client's own keys are
        /// Ctrl+Alt+Shift+Q (quit), Ctrl+Alt+Shift+F (fullscreen) and
        /// Ctrl+Alt+Shift+←/→ (the host's other monitors).
        /// Returns whether the key was used here.
        fn command(&mut self, event_loop: &ActiveEventLoop, key: &KeyEvent) -> bool {
            if key.state != ElementState::Pressed || key.repeat {
                return false;
            }
            let PhysicalKey::Code(code) = key.physical_key else {
                return false;
            };
            let chord = self.modifiers.control_key()
                && self.modifiers.alt_key()
                && self.modifiers.shift_key();
            match (self.input.is_some(), chord, code) {
                (true, true, KeyCode::KeyQ) | (false, _, KeyCode::Escape) => {
                    self.close(event_loop);
                    true
                }
                (true, true, KeyCode::KeyF) | (false, _, KeyCode::F11) => {
                    self.toggle_fullscreen();
                    true
                }
                (true, true, KeyCode::KeyM) => {
                    self.capture(!self.captured);
                    true
                }
                (_, true, KeyCode::ArrowRight) | (false, _, KeyCode::PageDown) => {
                    self.monitors
                        .request(fernsicht_client::MonitorRequest::Next);
                    true
                }
                (_, true, KeyCode::ArrowLeft) | (false, _, KeyCode::PageUp) => {
                    self.monitors
                        .request(fernsicht_client::MonitorRequest::Previous);
                    true
                }
                _ => false,
            }
        }

        fn scroll(&mut self, delta: MouseScrollDelta) -> InputEvent {
            // winit: positive = content moves right/down. Linux wheels:
            // positive = up and right. So y keeps its sign, x flips.
            let (x, y) = match delta {
                MouseScrollDelta::LineDelta(x, y) => (f64::from(x) * 120.0, f64::from(y) * 120.0),
                MouseScrollDelta::PixelDelta(p) => (p.x * WHEEL_PER_PIXEL, p.y * WHEEL_PER_PIXEL),
            };
            let (rx, ry) = (self.scroll_rest.0 - x, self.scroll_rest.1 + y);
            let (dx, dy) = (rx.trunc(), ry.trunc());
            self.scroll_rest = (rx - dx, ry - dy);
            InputEvent::Scroll {
                dx: dx as i32,
                dy: dy as i32,
            }
        }
    }

    impl ApplicationHandler<UserEvent> for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let Some(cfg) = self.cfg.take() else { return };
            let title = format!("Fernsicht – {}", cfg.host);
            let attrs = Window::default_attributes()
                .with_title(&title)
                .with_inner_size(LogicalSize::new(1280.0, 720.0));
            let window = match event_loop.create_window(attrs) {
                Ok(w) => Arc::new(w),
                Err(e) => {
                    self.error = Some(anyhow::anyhow!("create window: {e}"));
                    event_loop.exit();
                    return;
                }
            };
            if self.input.is_some() {
                // The host's pointer is drawn in the picture instead.
                window.set_cursor_visible(false);
                log::info!(
                    "mouse and keyboard go to the host; Ctrl+Alt+Shift+Q quits, \
                     Ctrl+Alt+Shift+F toggles fullscreen"
                );
            }
            if self.input.is_some() {
                self.shortcuts = crate::shortcuts::ShortcutLock::new(&window);
            }
            let for_presenter = window.clone();
            let factory: PresenterFactory = Box::new(move || {
                WindowPresenter::new(for_presenter, &title)
                    .map(|p| Box::new(p) as Box<dyn Presenter>)
            });
            let (stop, proxy) = (self.stop.clone(), self.proxy.clone());
            self.client = Some(std::thread::spawn(move || {
                let r = run_with(cfg, stop, factory);
                let _ = proxy.send_event(UserEvent::Done);
                r
            }));
            self.window = Some(window);
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested => self.close(event_loop),
                WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
                WindowEvent::Focused(false) => {
                    // Keys released while another window has the focus never
                    // reach us: let go of everything now.
                    if let Some(input) = &self.input {
                        input.release_all();
                    }
                    self.focused = false;
                    self.capture(false);
                    self.update_shortcuts();
                }
                WindowEvent::Focused(true) => {
                    self.focused = true;
                    self.update_shortcuts();
                }
                // Entering or leaving fullscreen shows as a new size.
                WindowEvent::Resized(_) => self.update_shortcuts(),
                WindowEvent::KeyboardInput { event: key, .. } => {
                    if self.command(event_loop, &key) {
                        return;
                    }
                    if let Some(input) = &self.input
                        && let Some(code) = key.physical_key.to_scancode()
                        && let Ok(code) = u16::try_from(code)
                        && (1..=fernsicht_proto::KEY_MAX).contains(&code)
                    {
                        input.push(InputEvent::Key {
                            code,
                            pressed: key.state == ElementState::Pressed,
                        });
                    }
                }
                WindowEvent::CursorMoved { position, .. } if !self.captured => {
                    if let (Some(input), Some(w)) = (&self.input, &self.window) {
                        let size = w.inner_size();
                        if let Some(e) =
                            input.pointer_at((position.x, position.y), (size.width, size.height))
                        {
                            input.push(e);
                        }
                    }
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    // In gaming mode the first click captures the pointer
                    // and stays here.
                    if self.gaming && !self.captured && self.input.is_some() {
                        if state == ElementState::Pressed {
                            self.capture(true);
                        }
                        return;
                    }
                    if let (Some(input), Some(code)) = (&self.input, mouse_button(button)) {
                        input.push(InputEvent::Button {
                            code,
                            pressed: state == ElementState::Pressed,
                        });
                    }
                }
                WindowEvent::MouseWheel { delta, .. } if self.input.is_some() => {
                    let e = self.scroll(delta);
                    if let (Some(input), InputEvent::Scroll { dx, dy }) = (&self.input, e)
                        && (dx != 0 || dy != 0)
                    {
                        input.push(e);
                    }
                }
                _ => {}
            }
        }

        fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
            if let (true, Some(input), DeviceEvent::MouseMotion { delta }) =
                (self.captured, &self.input, event)
            {
                let (rx, ry) = (self.motion_rest.0 + delta.0, self.motion_rest.1 + delta.1);
                let (dx, dy) = (rx.trunc(), ry.trunc());
                self.motion_rest = (rx - dx, ry - dy);
                if dx != 0.0 || dy != 0.0 {
                    input.push(InputEvent::MouseRel {
                        dx: dx as i32,
                        dy: dy as i32,
                    });
                }
            }
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, e: UserEvent) {
            match e {
                UserEvent::Done => event_loop.exit(),
                UserEvent::Keys(codes) => {
                    if let Some(input) = &self.input {
                        for e in fernsicht_client::chord_events(&codes) {
                            input.push(e);
                        }
                    }
                }
                UserEvent::Gaming(on) => {
                    self.gaming = on;
                    if !on {
                        self.capture(false);
                    }
                }
            }
        }
    }

    /// Runs the client in a window until it is closed, the client ends or
    /// `stop` is set.
    pub fn run(
        mut cfg: ClientConfig,
        options: Options,
        stop: Arc<AtomicBool>,
    ) -> anyhow::Result<RunSummary> {
        let input = options.send_input.then(|| Arc::new(InputHandle::default()));
        cfg.input = input.clone();
        let monitors = cfg.monitors.clone();
        let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
        // Mode switches from the app arrive on another thread.
        let proxy = event_loop.create_proxy();
        let commands = options.commands;
        std::thread::spawn(move || {
            for command in commands {
                let event = match command {
                    crate::AppCommand::Mode(gaming) => UserEvent::Gaming(gaming),
                    crate::AppCommand::Keys(codes) => UserEvent::Keys(codes),
                };
                if proxy.send_event(event).is_err() {
                    return;
                }
            }
        });
        let _gamepads = match (&input, options.gamepads) {
            (Some(input), true) => {
                let input = input.clone();
                fernsicht_input::gamepad::Gamepads::start("/dev/input".into(), true, move |e| {
                    input.push(e)
                })
                .map_err(|e| log::warn!("no gamepads: {e}"))
                .ok()
            }
            _ => None,
        };
        let mut app = App {
            cfg: Some(cfg),
            stop,
            proxy: event_loop.create_proxy(),
            window: None,
            client: None,
            error: None,
            input,
            modifiers: ModifiersState::empty(),
            scroll_rest: (0.0, 0.0),
            gaming: options.gaming,
            captured: false,
            motion_rest: (0.0, 0.0),
            focused: false,
            shortcuts: None,
            monitors,
        };
        event_loop.run_app(&mut app)?;
        app.stop.store(true, Ordering::Relaxed);
        if let Some(e) = app.error {
            return Err(e);
        }
        let client = app
            .client
            .take()
            .ok_or_else(|| anyhow::anyhow!("no window was opened"))?;
        client.join().expect("client thread panicked")
    }
}
