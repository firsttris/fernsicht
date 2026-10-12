//! The client's window: winit events to input for the host, the pointer
//! captured in gaming mode, and the presenter on the window's surface.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use fernsicht_client::{
    ClientConfig, InputEvent, InputHandle, PresenterFactory, RunSummary, buttons, run_with,
};
use fernsicht_render::Presenter;
use fernsicht_render::vulkan::Prefer;
use fernsicht_render::vulkan::window::WindowPresenter;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{
    DeviceEvent, DeviceId, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::window::{CursorGrabMode, CustomCursor, Fullscreen, Window, WindowId};

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
    /// Passes the desktop's shortcuts (Meta, Alt+Tab) to the host.
    /// Declared before `window`, so it is dropped first: on Wayland it
    /// talks to winit's display connection, which goes with the window
    /// once the event loop is gone.
    shortcuts: Option<crate::shortcuts::ShortcutLock>,
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
    /// The same for the presenter: in gaming mode frames show without
    /// waiting for the refresh (they may tear).
    tearing: Arc<AtomicBool>,
    /// The pointer is captured: motion goes to the host relatively.
    captured: bool,
    /// Fractions of pixels of relative motion, carried over.
    motion_rest: (f64, f64),
    focused: bool,
    /// The same for the gamepad thread: pads follow the focus.
    pads_focused: Arc<AtomicBool>,
    /// The host's monitors; Ctrl+Alt+Shift+←/→ switches.
    monitors: Arc<fernsicht_client::MonitorControl>,
    /// The host's pointer, shown as this window's own pointer once its
    /// hotspot is known (it then moves here at once).
    pointer: Arc<fernsicht_client::PointerShare>,
    /// The pointer image set now: serial, hotspot, scale in ‰.
    shown_pointer: Option<(u32, (u16, u16), u32)>,
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

/// Runs its function when dropped, also when the thread unwinds.
struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

impl App {
    fn close(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(input) = &self.input {
            input.release_all();
        }
        // Release: the network thread sends the releases queued above
        // before it says goodbye.
        self.stop.store(true, Ordering::Release);
        event_loop.exit();
    }

    /// Shows the host's pointer as this window's own while it can: with
    /// input going to the host, not captured, and the host's hotspot known.
    /// Otherwise the pointer is drawn into the picture where the host has
    /// it, a round trip later.
    fn update_pointer(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(w), Some(input)) = (&self.window, &self.input) else {
            return;
        };
        let Some(p) = (!self.captured).then(|| self.pointer.local()).flatten() else {
            if self.shown_pointer.take().is_some() {
                w.set_cursor_visible(false);
                self.pointer.set_shown_locally(false);
            }
            return;
        };
        let size = w.inner_size();
        let scale = input.stream_size().map_or(1.0, |s| {
            fernsicht_client::picture_scale((size.width, size.height), s, p.screen)
        });
        let key = (p.serial, p.hotspot, (scale * 1000.0).round() as u32);
        if self.shown_pointer == Some(key) {
            return;
        }
        let (width, height, hot, rgba) =
            fernsicht_client::system_cursor(&p.image, p.hotspot, scale);
        match CustomCursor::from_rgba(rgba, width, height, hot.0, hot.1) {
            Ok(source) => {
                w.set_cursor(event_loop.create_custom_cursor(source));
                w.set_cursor_visible(true);
                self.pointer.set_shown_locally(true);
                self.shown_pointer = Some(key);
            }
            Err(e) => log::debug!("pointer image: {e}"),
        }
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
        let chord =
            self.modifiers.control_key() && self.modifiers.alt_key() && self.modifiers.shift_key();
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
            // The host's pointer is drawn in the picture instead, until
            // the window can show it as its own (`update_pointer`).
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
        // Vulkan on the VAAPI decoder's GPU: its pictures import
        // without a copy; the Vulkan decoder runs on the window's GPU
        // (NVDEC pictures come through the CPU anyway).
        let prefer = match cfg.decoder {
            fernsicht_client::DecoderChoice::Nvdec => Prefer::Best,
            _ => Prefer::render_node(&cfg.render_node),
        };
        let tearing = self.tearing.clone();
        let factory: PresenterFactory = Box::new(move || {
            WindowPresenter::new(for_presenter, &title, prefer, tearing)
                .map(|p| Box::new(p) as Box<dyn Presenter>)
        });
        let (stop, proxy) = (self.stop.clone(), self.proxy.clone());
        self.client = Some(std::thread::spawn(move || {
            // Also when the session panics: the window closes instead
            // of freezing, and the panic shows at the join.
            let _done = OnDrop(Some(move || {
                let _ = proxy.send_event(UserEvent::Done);
            }));
            run_with(cfg, stop, factory)
        }));
        self.window = Some(window);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.update_pointer(event_loop);
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
                self.pads_focused.store(false, Ordering::Release);
                self.capture(false);
                self.update_shortcuts();
            }
            WindowEvent::Focused(true) => {
                self.focused = true;
                self.pads_focused.store(true, Ordering::Release);
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
                // and stays here. A release still goes on: the button
                // may have been pressed before the pointer was let go
                // (Ctrl+Alt+Shift+M), and must not stay down on the
                // host (a release of a button that is up does nothing).
                if self.gaming
                    && !self.captured
                    && self.input.is_some()
                    && state == ElementState::Pressed
                {
                    self.capture(true);
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
                self.tearing.store(on, Ordering::Relaxed);
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
    let pointer = cfg.pointer.clone();
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
    let pads_focused = Arc::new(AtomicBool::new(false));
    let _gamepads = match (&input, options.gamepads) {
        (Some(input), true) => fernsicht_input::gamepad::Gamepads::start(
            "/dev/input".into(),
            true,
            fernsicht_client::pad_events_while_focused(input.clone(), pads_focused.clone()),
        )
        .map_err(|e| log::warn!("no gamepads: {e}"))
        .ok()
        .map(|pads| {
            log::info!("gamepads through {}", pads.backend());
            // The host's games rumble the pads here.
            let pads = Arc::new(pads);
            let to = pads.clone();
            input.set_rumble(Arc::new(move |r| to.rumble(r)));
            pads
        }),
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
        tearing: Arc::new(AtomicBool::new(options.gaming)),
        captured: false,
        motion_rest: (0.0, 0.0),
        focused: false,
        pads_focused,
        shortcuts: None,
        monitors,
        pointer,
        shown_pointer: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_session_still_closes_the_window() {
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let _done = OnDrop(Some(move || tx.send("done").unwrap()));
            panic!("the session broke");
        });
        assert!(t.join().is_err());
        assert_eq!(rx.recv().unwrap(), "done");
    }
}
