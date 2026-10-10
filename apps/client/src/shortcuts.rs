//! System shortcuts (Meta, Meta+W, Alt+Tab, Ctrl+Alt+Del) for the host.
//!
//! The desktop the client runs on takes such keys before any window sees
//! them. While the session window has the focus and is fullscreen or holds
//! the pointer, it asks the desktop to pass them on:
//!
//! - Wayland: `zwp_keyboard_shortcuts_inhibit_manager_v1`, on winit's own
//!   connection. KDE asks the user once whether to allow it.
//! - X11: a keyboard grab.
//!
//! The client's own chords (Ctrl+Alt+Shift+F, M, Q) still work: the keys
//! reach the window, and the window handles those before forwarding.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1 as Manager,
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1 as Inhibitor},
};
use winit::window::Window;

/// Whether system shortcuts should go to the host: only while input goes
/// there and the window clearly owns the keyboard. In a plain window,
/// Alt+Tab stays with this desktop, or there would be no way out.
pub fn wanted(focused: bool, sends_input: bool, fullscreen: bool, captured: bool) -> bool {
    focused && sends_input && (fullscreen || captured)
}

pub struct ShortcutLock {
    backend: Backend,
    on: bool,
}

enum Backend {
    Wayland(Box<WaylandLock>),
    // Xlib's function table is large.
    X11(Box<X11Lock>),
}

impl ShortcutLock {
    /// For `window`; `None` if the desktop offers no way to do it.
    pub fn new(window: &Window) -> Option<Self> {
        let display = window.display_handle().ok()?.as_raw();
        let handle = window.window_handle().ok()?.as_raw();
        let backend = match (display, handle) {
            (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) => {
                match WaylandLock::new(d.display.as_ptr(), w.surface.as_ptr()) {
                    Ok(l) => Backend::Wayland(Box::new(l)),
                    Err(e) => {
                        log::info!("system shortcuts stay with this desktop: {e}");
                        return None;
                    }
                }
            }
            (RawDisplayHandle::Xlib(d), RawWindowHandle::Xlib(w)) => {
                let display = d.display?.as_ptr();
                Backend::X11(Box::new(X11Lock::new(display.cast(), w.window)?))
            }
            _ => return None,
        };
        Some(Self { backend, on: false })
    }

    /// Passes system shortcuts to the window (`true`) or gives them back.
    pub fn set(&mut self, on: bool) {
        if on == self.on {
            return;
        }
        let done = match &mut self.backend {
            Backend::Wayland(l) => l.set(on),
            Backend::X11(l) => l.set(on),
        };
        match done {
            Ok(()) => {
                self.on = on;
                if on {
                    log::info!(
                        "system shortcuts (Meta, Alt+Tab, …) go to the host; \
                         Ctrl+Alt+Shift+F leaves fullscreen"
                    );
                }
            }
            Err(e) => log::warn!("system shortcuts: {e}"),
        }
    }
}

struct State;

impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(State: ignore WlSeat);
wayland_client::delegate_noop!(State: ignore WlSurface);
wayland_client::delegate_noop!(State: Manager);

impl Dispatch<Inhibitor, ()> for State {
    fn event(
        _: &mut Self,
        _: &Inhibitor,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => {
                log::debug!("the desktop passes system shortcuts on")
            }
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                log::info!("the desktop keeps its shortcuts (not allowed, or focus moved)")
            }
            _ => {}
        }
    }
}

struct WaylandLock {
    conn: Connection,
    queue: EventQueue<State>,
    manager: Manager,
    seat: WlSeat,
    surface: WlSurface,
    inhibitor: Option<Inhibitor>,
}

impl WaylandLock {
    fn new(display: *mut std::ffi::c_void, surface: *mut std::ffi::c_void) -> Result<Self, String> {
        // SAFETY: winit's display outlives the window this lock belongs to;
        // a foreign backend does not close it.
        let backend =
            unsafe { wayland_backend::client::Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = wayland_client::globals::registry_queue_init::<State>(&conn)
            .map_err(|e| format!("Wayland registry: {e}"))?;
        let qh = queue.handle();
        let manager: Manager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| "the compositor has no keyboard-shortcuts-inhibit".to_string())?;
        let seat: WlSeat = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| format!("no seat: {e}"))?;
        // SAFETY: the pointer is winit's live wl_surface of this window.
        let id = unsafe {
            wayland_backend::client::ObjectId::from_ptr(WlSurface::interface(), surface.cast())
        }
        .map_err(|e| format!("window surface: {e:?}"))?;
        let surface = WlSurface::from_id(&conn, id).map_err(|e| format!("window surface: {e}"))?;
        Ok(Self {
            conn,
            queue,
            manager,
            seat,
            surface,
            inhibitor: None,
        })
    }

    fn set(&mut self, on: bool) -> Result<(), String> {
        if on {
            let qh = self.queue.handle();
            self.inhibitor =
                Some(
                    self.manager
                        .inhibit_shortcuts(&self.surface, &self.seat, &qh, ()),
                );
        } else if let Some(i) = self.inhibitor.take() {
            i.destroy();
        }
        self.conn.flush().map_err(|e| format!("Wayland: {e}"))?;
        // Reads the answer (active or not) for the log; harmless otherwise.
        self.queue
            .roundtrip(&mut State)
            .map(|_| ())
            .map_err(|e| format!("Wayland: {e}"))
    }
}

impl Drop for WaylandLock {
    fn drop(&mut self) {
        if let Some(i) = self.inhibitor.take() {
            i.destroy();
        }
        self.manager.destroy();
        let _ = self.conn.flush();
    }
}

struct X11Lock {
    xlib: x11_dl::xlib::Xlib,
    display: *mut x11_dl::xlib::Display,
    window: x11_dl::xlib::Window,
}

impl X11Lock {
    fn new(display: *mut x11_dl::xlib::Display, window: std::os::raw::c_ulong) -> Option<Self> {
        let xlib = x11_dl::xlib::Xlib::open().ok()?;
        Some(Self {
            xlib,
            display,
            window,
        })
    }

    fn set(&mut self, on: bool) -> Result<(), String> {
        // SAFETY: winit's live display and window; Xlib calls on the thread
        // that runs the event loop.
        unsafe {
            if on {
                let r = (self.xlib.XGrabKeyboard)(
                    self.display,
                    self.window,
                    x11_dl::xlib::True,
                    x11_dl::xlib::GrabModeAsync,
                    x11_dl::xlib::GrabModeAsync,
                    x11_dl::xlib::CurrentTime,
                );
                (self.xlib.XFlush)(self.display);
                if r != x11_dl::xlib::GrabSuccess {
                    return Err(format!("keyboard grab refused ({r})"));
                }
            } else {
                (self.xlib.XUngrabKeyboard)(self.display, x11_dl::xlib::CurrentTime);
                (self.xlib.XFlush)(self.display);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_go_to_the_host_only_when_the_window_owns_the_keyboard() {
        // focused, sends input, fullscreen, captured
        assert!(wanted(true, true, true, false), "fullscreen");
        assert!(wanted(true, true, false, true), "pointer captured");
        assert!(
            !wanted(true, true, false, false),
            "a plain window keeps Alt+Tab"
        );
        assert!(!wanted(false, true, true, true), "not focused");
        assert!(!wanted(true, false, true, true), "view only");
    }
}
