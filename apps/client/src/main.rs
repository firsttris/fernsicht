use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use clap::Parser;
use fernsicht_client::{ClientConfig, DecoderChoice, run};
use fernsicht_render::overlay::ms;

/// Fernsicht client: shows the host's screen in a window (build feature
/// "window"; Esc closes, F11 toggles fullscreen) and prints the latency
/// overlay.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Host agent address, e.g. 192.168.1.20:47800.
    host: String,
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
    /// Save the received video to this file (H.264: `ffplay file.h264`).
    #[arg(long)]
    record: Option<std::path::PathBuf>,
    /// H.264 decoder: "auto" (VAAPI, else NVDEC), "vaapi" or "nvdec".
    #[arg(long, value_enum, default_value_t = DecoderArg::Auto)]
    decoder: DecoderArg,
    /// No window: decode only and print the overlay.
    #[arg(long)]
    headless: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum DecoderArg {
    Auto,
    Vaapi,
    Nvdec,
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
        decoder: match args.decoder {
            DecoderArg::Auto => DecoderChoice::Auto,
            DecoderArg::Vaapi => DecoderChoice::Vaapi,
            DecoderArg::Nvdec => DecoderChoice::Nvdec,
        },
        record: args.record,
        ..ClientConfig::default()
    };
    let s = if args.headless || !cfg!(feature = "window") {
        if !args.headless {
            log::info!("built without the \"window\" feature: running headless");
        }
        run(cfg, Arc::new(AtomicBool::new(false)))?
    } else {
        #[cfg(feature = "window")]
        {
            window::run(cfg)?
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

#[cfg(feature = "window")]
mod window {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;

    use fernsicht_client::{ClientConfig, PresenterFactory, RunSummary, run_with};
    use fernsicht_render::Presenter;
    use fernsicht_render::vulkan::window::WindowPresenter;
    use winit::application::ApplicationHandler;
    use winit::dpi::LogicalSize;
    use winit::event::{ElementState, KeyEvent, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
    use winit::keyboard::{Key, NamedKey};
    use winit::window::{Fullscreen, Window, WindowId};

    /// The client thread has ended (duration over, host gone, error).
    struct Done;

    struct App {
        cfg: Option<ClientConfig>,
        stop: Arc<AtomicBool>,
        proxy: EventLoopProxy<Done>,
        window: Option<Arc<Window>>,
        client: Option<JoinHandle<anyhow::Result<RunSummary>>>,
        error: Option<anyhow::Error>,
    }

    impl App {
        fn close(&mut self, event_loop: &ActiveEventLoop) {
            self.stop.store(true, Ordering::Relaxed);
            event_loop.exit();
        }
    }

    impl ApplicationHandler<Done> for App {
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
            let for_presenter = window.clone();
            let factory: PresenterFactory = Box::new(move || {
                WindowPresenter::new(for_presenter, &title)
                    .map(|p| Box::new(p) as Box<dyn Presenter>)
            });
            let (stop, proxy) = (self.stop.clone(), self.proxy.clone());
            self.client = Some(std::thread::spawn(move || {
                let r = run_with(cfg, stop, factory);
                let _ = proxy.send_event(Done);
                r
            }));
            self.window = Some(window);
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::CloseRequested => self.close(event_loop),
                WindowEvent::KeyboardInput {
                    event:
                        KeyEvent {
                            logical_key: Key::Named(key),
                            state: ElementState::Pressed,
                            repeat: false,
                            ..
                        },
                    ..
                } => match key {
                    NamedKey::Escape => self.close(event_loop),
                    NamedKey::F11 => {
                        if let Some(w) = &self.window {
                            let full = w.fullscreen().is_some();
                            w.set_fullscreen((!full).then_some(Fullscreen::Borderless(None)));
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, _: Done) {
            event_loop.exit();
        }
    }

    pub fn run(cfg: ClientConfig) -> anyhow::Result<RunSummary> {
        let event_loop = EventLoop::<Done>::with_user_event().build()?;
        let mut app = App {
            cfg: Some(cfg),
            stop: Arc::new(AtomicBool::new(false)),
            proxy: event_loop.create_proxy(),
            window: None,
            client: None,
            error: None,
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
