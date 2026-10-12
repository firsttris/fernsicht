//! Gamepads through SDL3 against a virtual SDL gamepad: its buttons and
//! triggers arrive as protocol events, and a rumble reaches it. Skips
//! where SDL3 is not installed (`FERNSICHT_REQUIRE_SDL` makes it fail).
#![cfg(feature = "gamepad")]

use std::ffi::{c_char, c_int, c_void};
use std::sync::Mutex;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use fernsicht_input::InputEvent;
use fernsicht_input::gamepad::Gamepads;
use fernsicht_input::sdl::Rumble;
use fernsicht_proto::PadAxis;

/// `SDL_VirtualJoystickDesc` (SDL_joystick.h, SDL 3.2).
#[repr(C)]
struct VirtualDesc {
    version: u32,
    kind: u16,
    padding: u16,
    vendor_id: u16,
    product_id: u16,
    naxes: u16,
    nbuttons: u16,
    nballs: u16,
    nhats: u16,
    ntouchpads: u16,
    nsensors: u16,
    padding2: [u16; 2],
    button_mask: u32,
    axis_mask: u32,
    name: *const c_char,
    touchpads: *const c_void,
    sensors: *const c_void,
    userdata: *mut c_void,
    update: Option<unsafe extern "C" fn(*mut c_void)>,
    set_player_index: Option<unsafe extern "C" fn(*mut c_void, c_int)>,
    rumble: Option<unsafe extern "C" fn(*mut c_void, u16, u16) -> bool>,
    rumble_triggers: Option<unsafe extern "C" fn(*mut c_void, u16, u16) -> bool>,
    set_led: Option<unsafe extern "C" fn(*mut c_void, u8, u8, u8) -> bool>,
    send_effect: Option<unsafe extern "C" fn(*mut c_void, *const c_void, c_int) -> bool>,
    set_sensors_enabled: Option<unsafe extern "C" fn(*mut c_void, bool) -> bool>,
    cleanup: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// What the virtual pad's motors were last told.
static RUMBLED: Mutex<Option<(u16, u16)>> = Mutex::new(None);

unsafe extern "C" fn rumble(_: *mut c_void, low: u16, high: u16) -> bool {
    *RUMBLED.lock().unwrap() = Some((low, high));
    true
}

const SDL_INIT_JOYSTICK: u32 = 0x200;
const SDL_JOYSTICK_TYPE_GAMEPAD: u16 = 1;

struct Sdl {
    init_sub: unsafe extern "C" fn(u32) -> bool,
    attach: unsafe extern "C" fn(*const VirtualDesc) -> u32,
    detach: unsafe extern "C" fn(u32) -> bool,
    open: unsafe extern "C" fn(u32) -> *mut c_void,
    set_button: unsafe extern "C" fn(*mut c_void, c_int, bool) -> bool,
    set_axis: unsafe extern "C" fn(*mut c_void, c_int, i16) -> bool,
    _lib: libloading::Library,
}

fn sdl() -> Option<Sdl> {
    // SAFETY: SDL3's functions with their signatures (SDL_joystick.h).
    let loaded = unsafe {
        libloading::Library::new("libSDL3.so.0").and_then(|lib| {
            Ok(Sdl {
                init_sub: *lib.get(b"SDL_InitSubSystem\0")?,
                attach: *lib.get(b"SDL_AttachVirtualJoystick\0")?,
                detach: *lib.get(b"SDL_DetachVirtualJoystick\0")?,
                open: *lib.get(b"SDL_OpenJoystick\0")?,
                set_button: *lib.get(b"SDL_SetJoystickVirtualButton\0")?,
                set_axis: *lib.get(b"SDL_SetJoystickVirtualAxis\0")?,
                _lib: lib,
            })
        })
    };
    match loaded {
        Ok(s) => Some(s),
        Err(e) => {
            assert!(
                std::env::var_os("FERNSICHT_REQUIRE_SDL").is_none(),
                "SDL3 required: {e}"
            );
            eprintln!("skipped: no SDL3 ({e})");
            None
        }
    }
}

/// Waits up to two seconds for an event that `want` picks.
fn wait_for<T>(
    rx: &std::sync::mpsc::Receiver<InputEvent>,
    mut want: impl FnMut(&InputEvent) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let e = rx.recv_timeout(left).ok()?;
        if let Some(v) = want(&e) {
            return Some(v);
        }
    }
    None
}

#[test]
fn a_virtual_sdl_pad_plays_and_rumbles() {
    let Some(sdl) = sdl() else { return };
    let (tx, rx) = channel();
    let pads = Gamepads::start("/dev/input".into(), true, move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    assert_eq!(pads.backend(), "SDL");
    let desc = VirtualDesc {
        version: std::mem::size_of::<VirtualDesc>() as u32,
        kind: SDL_JOYSTICK_TYPE_GAMEPAD,
        padding: 0,
        vendor_id: 0,
        product_id: 0,
        naxes: 6,
        nbuttons: 15,
        nballs: 0,
        nhats: 0,
        ntouchpads: 0,
        nsensors: 0,
        padding2: [0; 2],
        button_mask: (1 << 15) - 1,
        axis_mask: (1 << 6) - 1,
        name: c"SDL test pad".as_ptr(),
        touchpads: std::ptr::null(),
        sensors: std::ptr::null(),
        userdata: std::ptr::null_mut(),
        update: None,
        set_player_index: None,
        rumble: Some(rumble),
        rumble_triggers: None,
        set_led: None,
        send_effect: None,
        set_sensors_enabled: None,
        cleanup: None,
    };
    // SAFETY: SDL's virtual joystick API; the description outlives the
    // call (SDL copies it), the joystick is detached at the end.
    unsafe {
        assert!((sdl.init_sub)(SDL_INIT_JOYSTICK));
        let id = (sdl.attach)(&desc);
        assert_ne!(id, 0, "virtual pad not attached");
        let js = (sdl.open)(id);
        assert!(!js.is_null());
        // South pressed, the right trigger pulled.
        (sdl.set_button)(js, 0, true);
        // SDL takes a trigger's first value after opening as where it
        // rests; the next one moves it.
        (sdl.set_axis)(js, 5, -32768);
        std::thread::sleep(Duration::from_millis(100));
        (sdl.set_axis)(js, 5, 32767);
        let pad = wait_for(&rx, |e| match *e {
            InputEvent::PadButton {
                pad,
                code: 0x130,
                pressed: true,
            } => Some(pad),
            _ => None,
        })
        .expect("the south button never came");
        let pulled = wait_for(&rx, |e| match *e {
            InputEvent::PadAxis {
                pad: p,
                axis: PadAxis::RightTrigger,
                value,
            } if p == pad => Some(value),
            _ => None,
        });
        assert_eq!(pulled, Some(255), "the trigger at its end");
        pads.rumble(Rumble {
            pad,
            low: 0x4000,
            high: 0x8000,
            ms: 200,
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while RUMBLED.lock().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*RUMBLED.lock().unwrap(), Some((0x4000, 0x8000)));
        // Unplugged: the button lets go.
        (sdl.detach)(id);
        let released = wait_for(&rx, |e| match *e {
            InputEvent::PadButton {
                pad: p,
                code: 0x130,
                pressed: false,
            } if p == pad => Some(()),
            _ => None,
        });
        assert!(released.is_some(), "an unplugged pad keeps its button");
    }
}
