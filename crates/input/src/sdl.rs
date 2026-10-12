//! Gamepads through SDL3 (D9), loaded at runtime (`libSDL3.so.0`): SDL's
//! controller database names every button of hundreds of pads by its
//! place (south, east, ...), and it plays rumble on them. Where SDL3 is
//! missing, [`crate::gamepad`] reads evdev as before.
//!
//! One thread owns SDL's gamepad subsystem. It sleeps in
//! `SDL_WaitEventTimeout` until a pad changes, then reads every open
//! pad's buttons and axes and hands them through the same [`PadState`]
//! as evdev, so the protocol events (and their deduplication, and the
//! release when a pad goes) are the same. SDL's buttons and axes are
//! standardised; [`button_code`] and [`axis_code`] put them on the evdev
//! codes `PadState` reads. Rumble requests come through a channel and
//! wake the thread with a user event.
//!
//! The window is not SDL's, so SDL has to be told to deliver pad events
//! without focus (`SDL_HINT_JOYSTICK_ALLOW_BACKGROUND_EVENTS`); the
//! client itself decides whether its window has focus.

use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use fernsicht_proto::{InputEvent, MAX_PADS, PadAxis};

pub use crate::Rumble;

use crate::gamepad::{
    ABS_RX, ABS_RY, ABS_RZ, ABS_X, ABS_Y, ABS_Z, AxisMap, BTN_DPAD_DOWN, BTN_DPAD_LEFT,
    BTN_DPAD_RIGHT, BTN_DPAD_UP, BTN_NORTH, BTN_SOUTH, BTN_WEST, EV_ABS, EV_KEY, Mapping, PadState,
};

// SDL_init.h, SDL_events.h, SDL_gamepad.h, SDL_hints.h (SDL 3.2).
const SDL_INIT_GAMEPAD: u32 = 0x0000_2000;
const SDL_EVENT_GAMEPAD_ADDED: u32 = 0x653;
const SDL_EVENT_GAMEPAD_REMOVED: u32 = 0x654;
const SDL_EVENT_USER: u32 = 0x8000;
const HINT_BACKGROUND: &CStr = c"SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS";
/// `SDL_GAMEPAD_BUTTON_SOUTH` .. `SDL_GAMEPAD_BUTTON_DPAD_RIGHT`.
const BUTTONS: std::ops::Range<c_int> = 0..15;
/// `SDL_GAMEPAD_AXIS_LEFTX` .. `SDL_GAMEPAD_AXIS_RIGHT_TRIGGER`.
const AXES: std::ops::Range<c_int> = 0..6;

const BTN_EAST: u16 = 0x131;
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_THUMBL: u16 = 0x13d;
const BTN_THUMBR: u16 = 0x13e;

/// The evdev button code of an SDL gamepad button, by place.
pub fn button_code(button: c_int) -> Option<u16> {
    Some(match button {
        0 => BTN_SOUTH,
        1 => BTN_EAST,
        2 => BTN_WEST,
        3 => BTN_NORTH,
        4 => BTN_SELECT,
        5 => BTN_MODE,
        6 => BTN_START,
        7 => BTN_THUMBL,
        8 => BTN_THUMBR,
        9 => BTN_TL,
        10 => BTN_TR,
        11 => BTN_DPAD_UP,
        12 => BTN_DPAD_DOWN,
        13 => BTN_DPAD_LEFT,
        14 => BTN_DPAD_RIGHT,
        _ => return None,
    })
}

/// The evdev axis code of an SDL gamepad axis (Xbox layout: the right
/// stick on RX/RY, the triggers on Z/RZ).
pub fn axis_code(axis: c_int) -> Option<u16> {
    Some(match axis {
        0 => ABS_X,
        1 => ABS_Y,
        2 => ABS_RX,
        3 => ABS_RY,
        4 => ABS_Z,
        5 => ABS_RZ,
        _ => return None,
    })
}

/// How SDL's values read: sticks −32768..32767, triggers 0..32767.
pub fn mapping() -> Mapping {
    let stick = |axis| AxisMap {
        axis,
        min: -32768,
        max: 32767,
    };
    let trigger = |axis| AxisMap {
        axis,
        min: 0,
        max: 32767,
    };
    let axes = BTreeMap::from([
        (ABS_X, stick(PadAxis::LeftX)),
        (ABS_Y, stick(PadAxis::LeftY)),
        (ABS_RX, stick(PadAxis::RightX)),
        (ABS_RY, stick(PadAxis::RightY)),
        (ABS_Z, trigger(PadAxis::LeftTrigger)),
        (ABS_RZ, trigger(PadAxis::RightTrigger)),
    ]);
    Mapping {
        axes,
        swap_xy: false,
        digital_triggers: false,
    }
}

type GamepadPtr = *mut c_void;

/// The SDL functions used, loaded once per process.
struct Api {
    init_sub: unsafe extern "C" fn(u32) -> bool,
    quit_sub: unsafe extern "C" fn(u32),
    set_hint: unsafe extern "C" fn(*const c_char, *const c_char) -> bool,
    get_error: unsafe extern "C" fn() -> *const c_char,
    get_gamepads: unsafe extern "C" fn(*mut c_int) -> *mut u32,
    open: unsafe extern "C" fn(u32) -> GamepadPtr,
    close: unsafe extern "C" fn(GamepadPtr),
    name: unsafe extern "C" fn(GamepadPtr) -> *const c_char,
    path: unsafe extern "C" fn(GamepadPtr) -> *const c_char,
    button: unsafe extern "C" fn(GamepadPtr, c_int) -> bool,
    axis: unsafe extern "C" fn(GamepadPtr, c_int) -> i16,
    rumble: unsafe extern "C" fn(GamepadPtr, u16, u16, u32) -> bool,
    wait_event: unsafe extern "C" fn(*mut Event, i32) -> bool,
    push_event: unsafe extern "C" fn(*mut Event) -> bool,
    free: unsafe extern "C" fn(*mut c_void),
    _lib: libloading::Library,
}

/// `SDL_Event`: a 128-byte union; only its type (the first u32) is read.
#[repr(C, align(8))]
struct Event([u8; 128]);

impl Event {
    fn of(kind: u32) -> Self {
        let mut e = Event([0; 128]);
        e.0[..4].copy_from_slice(&kind.to_ne_bytes());
        e
    }

    fn kind(&self) -> u32 {
        u32::from_ne_bytes([self.0[0], self.0[1], self.0[2], self.0[3]])
    }
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: loading SDL3, whose symbols have the signatures above
        // (SDL_init.h, SDL_gamepad.h, SDL_events.h, SDL_hints.h,
        // SDL_error.h, SDL_stdinc.h).
        unsafe {
            let lib = libloading::Library::new("libSDL3.so.0")
                .map_err(|e| format!("no SDL3 (libSDL3.so.0): {e}"))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get($name)
                        .map_err(|e| format!("SDL3 lacks {}: {e}", stringify!($name)))?
                };
            }
            Ok(Api {
                init_sub: sym!(b"SDL_InitSubSystem\0"),
                quit_sub: sym!(b"SDL_QuitSubSystem\0"),
                set_hint: sym!(b"SDL_SetHint\0"),
                get_error: sym!(b"SDL_GetError\0"),
                get_gamepads: sym!(b"SDL_GetGamepads\0"),
                open: sym!(b"SDL_OpenGamepad\0"),
                close: sym!(b"SDL_CloseGamepad\0"),
                name: sym!(b"SDL_GetGamepadName\0"),
                path: sym!(b"SDL_GetGamepadPath\0"),
                button: sym!(b"SDL_GetGamepadButton\0"),
                axis: sym!(b"SDL_GetGamepadAxis\0"),
                rumble: sym!(b"SDL_RumbleGamepad\0"),
                wait_event: sym!(b"SDL_WaitEventTimeout\0"),
                push_event: sym!(b"SDL_PushEvent\0"),
                free: sym!(b"SDL_free\0"),
                _lib: lib,
            })
        }
    })
    .as_ref()
    .map_err(Clone::clone)
}

fn sdl_error(api: &Api) -> String {
    // SAFETY: SDL_GetError returns a NUL-terminated string (maybe empty).
    unsafe { CStr::from_ptr((api.get_error)()) }
        .to_string_lossy()
        .into_owned()
}

/// Wakes the SDL thread for a rumble.
pub(crate) struct Waker;

impl Waker {
    pub(crate) fn wake(&self) {
        if let Ok(api) = api() {
            let mut e = Event::of(SDL_EVENT_USER);
            // SAFETY: SDL_PushEvent copies the event; thread-safe.
            unsafe { (api.push_event)(&mut e) };
        }
    }
}

struct OpenPad {
    id: u32,
    handle: GamepadPtr,
    slot: u8,
    state: PadState,
}

/// The SDL thread's state: SDL is initialised (and quit on drop).
struct Session {
    api: &'static Api,
    pads: Vec<OpenPad>,
    map: Mapping,
    skip_own: bool,
}

impl Session {
    fn start(skip_own: bool) -> Result<Self, String> {
        let api = api()?;
        // SAFETY: plain calls with NUL-terminated strings.
        unsafe {
            (api.set_hint)(HINT_BACKGROUND.as_ptr(), c"1".as_ptr());
            if !(api.init_sub)(SDL_INIT_GAMEPAD) {
                return Err(format!("SDL gamepads: {}", sdl_error(api)));
            }
        }
        Ok(Self {
            api,
            pads: Vec::new(),
            map: mapping(),
            skip_own,
        })
    }

    /// Opens pads that came, lets go of those that went.
    fn rescan(&mut self, sink: &mut impl FnMut(InputEvent)) {
        let mut n = 0;
        // SAFETY: SDL hands out an array of n ids, freed with SDL_free.
        let ids: Vec<u32> = unsafe {
            let p = (self.api.get_gamepads)(&mut n);
            if p.is_null() {
                return;
            }
            let ids = std::slice::from_raw_parts(p, n.max(0) as usize).to_vec();
            (self.api.free)(p.cast());
            ids
        };
        let (api, pads) = (self.api, &mut self.pads);
        pads.retain_mut(|p| {
            let here = ids.contains(&p.id);
            if !here {
                log::info!("gamepad {} unplugged", p.slot + 1);
                p.state.release(p.slot).into_iter().for_each(&mut *sink);
                // SAFETY: opened by us, closed once.
                unsafe { (api.close)(p.handle) };
            }
            here
        });
        for id in ids {
            if pads.iter().any(|p| p.id == id) {
                continue;
            }
            let Some(slot) = (0..MAX_PADS).find(|s| pads.iter().all(|p| p.slot != *s)) else {
                break;
            };
            // SAFETY: an id SDL just listed.
            let handle = unsafe { (api.open)(id) };
            if handle.is_null() {
                continue;
            }
            let text = |f: unsafe extern "C" fn(GamepadPtr) -> *const c_char| {
                // SAFETY: SDL's strings live as long as the gamepad (or
                // are null).
                unsafe {
                    let s = f(handle);
                    (!s.is_null()).then(|| CStr::from_ptr(s).to_string_lossy().into_owned())
                }
            };
            let name = text(api.name).unwrap_or_default();
            // SDL names pads from its database ("Xbox 360 Controller"); the
            // kernel's name of the device behind the path tells our own.
            let own = crate::gamepad::is_own(&name)
                || text(api.path).is_some_and(|p| crate::gamepad::is_own_device(&p));
            if self.skip_own && own {
                // SAFETY: just opened.
                unsafe { (api.close)(handle) };
                continue;
            }
            log::info!("gamepad {} (SDL): {name}", slot + 1);
            pads.push(OpenPad {
                id,
                handle,
                slot,
                state: PadState::default(),
            });
        }
    }

    /// Reads every open pad and hands what changed to `sink`.
    fn read(&mut self, sink: &mut impl FnMut(InputEvent)) {
        for p in &mut self.pads {
            for b in BUTTONS {
                let Some(code) = button_code(b) else { continue };
                // SAFETY: an open gamepad.
                let down = unsafe { (self.api.button)(p.handle, b) };
                p.state
                    .translate(&self.map, p.slot, EV_KEY, code, i32::from(down), &mut *sink);
            }
            for a in AXES {
                let Some(code) = axis_code(a) else { continue };
                // SAFETY: an open gamepad.
                let v = unsafe { (self.api.axis)(p.handle, a) };
                p.state
                    .translate(&self.map, p.slot, EV_ABS, code, i32::from(v), &mut *sink);
            }
        }
    }

    fn rumble(&self, r: Rumble) {
        if let Some(p) = self.pads.iter().find(|p| p.slot == r.pad) {
            // SAFETY: an open gamepad.
            unsafe { (self.api.rumble)(p.handle, r.low, r.high, r.ms) };
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: opened by us; the subsystem was initialised in start().
        unsafe {
            for p in self.pads.drain(..) {
                (self.api.close)(p.handle);
            }
            (self.api.quit_sub)(SDL_INIT_GAMEPAD);
        }
    }
}

/// How long the thread sleeps at most between looks (stop flag, a pad
/// SDL did not announce).
const WAIT_MS: i32 = 100;

/// Runs SDL's gamepads on the calling thread until `stop`: `ready` gets
/// whether SDL started (the caller falls back to evdev if not).
pub(crate) fn run(
    skip_own: bool,
    stop: &std::sync::atomic::AtomicBool,
    rumbles: Receiver<Rumble>,
    ready: &Sender<Result<&'static str, String>>,
    sink: &mut impl FnMut(InputEvent),
) -> Result<(), String> {
    let mut session = Session::start(skip_own)?;
    let _ = ready.send(Ok("SDL"));
    let mut e = Event([0; 128]);
    let mut scanned: Option<Instant> = None;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        // SAFETY: a 128-byte, 8-aligned SDL_Event for SDL to fill.
        let got = unsafe { (session.api.wait_event)(&mut e, WAIT_MS) };
        let plugged = got
            && matches!(
                e.kind(),
                SDL_EVENT_GAMEPAD_ADDED | SDL_EVENT_GAMEPAD_REMOVED
            );
        if plugged || scanned.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
            session.rescan(sink);
            scanned = Some(Instant::now());
        }
        while let Ok(r) = rumbles.try_recv() {
            session.rumble(r);
        }
        session.read(sink);
    }
    for p in &mut session.pads {
        p.state.release(p.slot).into_iter().for_each(&mut *sink);
    }
    Ok(())
}

/// What [`crate::gamepad::Gamepads`] keeps to send rumbles to the SDL
/// thread.
pub(crate) struct Rumbles {
    pub(crate) tx: Sender<Rumble>,
    pub(crate) waker: Arc<Waker>,
}

pub(crate) fn rumble_channel() -> (Rumbles, Receiver<Rumble>) {
    let (tx, rx) = channel();
    (
        Rumbles {
            tx,
            waker: Arc::new(Waker),
        },
        rx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamepad::scale;

    #[test]
    fn buttons_keep_their_place() {
        // SDL's south, east, west, north are the evdev positions.
        assert_eq!(button_code(0), Some(BTN_SOUTH));
        assert_eq!(button_code(1), Some(BTN_EAST));
        assert_eq!(button_code(2), Some(BTN_WEST));
        assert_eq!(button_code(3), Some(BTN_NORTH));
        assert_eq!(button_code(6), Some(BTN_START));
        assert_eq!(button_code(14), Some(BTN_DPAD_RIGHT));
        assert_eq!(button_code(15), None, "misc buttons have no place");
        assert_eq!(button_code(-1), None);
        // Every face, shoulder, stick and menu button is a protocol pad
        // button; the d-pad goes through PadState's d-pad axes.
        for b in 0..11 {
            let code = button_code(b).unwrap();
            assert!(
                (fernsicht_proto::BTN_PAD_FIRST..=fernsicht_proto::BTN_PAD_LAST).contains(&code)
            );
        }
    }

    #[test]
    fn axes_scale_to_the_protocol() {
        let m = mapping();
        let left_x = &m.axes[&axis_code(0).unwrap()];
        assert_eq!(scale(left_x, -32768), -32768);
        assert_eq!(scale(left_x, 32767), 32767);
        let trigger = &m.axes[&axis_code(5).unwrap()];
        assert_eq!(trigger.axis, PadAxis::RightTrigger);
        assert_eq!(scale(trigger, 32767), 255);
        assert_eq!(scale(trigger, 0), 0);
        assert_eq!(axis_code(6), None);
    }

    #[test]
    fn pad_state_turns_sdl_reads_into_events_once() {
        let (m, mut state, mut out) = (mapping(), PadState::default(), Vec::new());
        let mut read = |kind, code, v| state.translate(&m, 0, kind, code, v, |e| out.push(e));
        read(EV_KEY, button_code(0).unwrap(), 1);
        read(EV_KEY, button_code(0).unwrap(), 1);
        read(EV_ABS, axis_code(4).unwrap(), 32767);
        read(EV_KEY, button_code(12).unwrap(), 1);
        assert_eq!(
            out,
            [
                InputEvent::PadButton {
                    pad: 0,
                    code: BTN_SOUTH,
                    pressed: true
                },
                InputEvent::PadAxis {
                    pad: 0,
                    axis: PadAxis::LeftTrigger,
                    value: 255
                },
                InputEvent::PadAxis {
                    pad: 0,
                    axis: PadAxis::DpadY,
                    value: 1
                },
            ],
            "held buttons are not repeated"
        );
    }
}
