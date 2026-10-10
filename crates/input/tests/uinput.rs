//! The uinput backend against the real kernel. It creates virtual devices
//! and sends key presses, so it only runs where that cannot disturb
//! anyone: with `FERNSICHT_UINPUT_TEST=1` (set in CI, on a machine without
//! a desktop). Never on a developer's machine by accident.
#![cfg(feature = "uinput")]

use std::time::Duration;

use fernsicht_input::uinput::{AbsArea, Uinput};
use fernsicht_input::{InputEvent, InputSink, buttons};

fn enabled() -> bool {
    let on = std::env::var_os("FERNSICHT_UINPUT_TEST").is_some();
    if !on {
        eprintln!("skipped: set FERNSICHT_UINPUT_TEST=1 (creates real input devices)");
    }
    on
}

/// The tests create and remove devices and look at the list: one at a
/// time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn devices() -> String {
    std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default()
}

#[test]
fn devices_are_created_take_every_event_and_go_away() {
    if !enabled() {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut u = Uinput::open(AbsArea {
        x: 0.5,
        y: 0.0,
        width: 0.5,
        height: 1.0,
    })
    .expect("open /dev/uinput");
    let listed = devices();
    for name in ["Fernsicht keyboard", "Fernsicht pointer", "Fernsicht mouse"] {
        assert!(listed.contains(name), "{name} missing:\n{listed}");
    }
    let events = [
        InputEvent::MouseAbs { x: 100, y: 65535 },
        InputEvent::Button {
            code: buttons::LEFT,
            pressed: true,
        },
        InputEvent::Scroll { dx: 0, dy: -120 },
        InputEvent::Scroll { dx: 30, dy: 0 },
        InputEvent::MouseRel { dx: 5, dy: -5 },
        InputEvent::Button {
            code: buttons::RIGHT,
            pressed: true,
        },
        InputEvent::Key {
            code: 30,
            pressed: true,
        },
        InputEvent::Key {
            code: 30,
            pressed: false,
        },
        InputEvent::Key {
            code: 42,
            pressed: true,
        },
    ];
    for e in &events {
        u.inject(e).unwrap_or_else(|err| panic!("{e:?}: {err}"));
    }
    // Shift and both buttons are still held: released here.
    u.release_all();
    drop(u);
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !devices().contains("Fernsicht keyboard"),
        "devices removed on drop"
    );
}

/// The host's virtual pad, read back by the client's gamepad reader: what
/// goes in by position comes out by position.
#[cfg(feature = "gamepad")]
#[test]
fn a_virtual_pad_reads_back_through_evdev() {
    use fernsicht_input::gamepad::Gamepads;
    use fernsicht_proto::PadAxis;
    use std::sync::{Arc, Mutex};
    if !enabled() {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut u = Uinput::open(AbsArea::default()).expect("open /dev/uinput");
    let north = InputEvent::PadButton {
        pad: 0,
        code: 0x133,
        pressed: true,
    };
    // The pad appears on first use.
    u.inject(&north).unwrap();
    assert!(devices().contains("Fernsicht X-Box 360 pad 1"));
    let got = Arc::new(Mutex::new(Vec::new()));
    let sink = got.clone();
    let reader = Gamepads::start("/dev/input".into(), false, move |e| {
        sink.lock().unwrap().push(e)
    })
    .unwrap();
    // udev may set the new device's permissions a moment later, and the
    // reader looks again every 2 s: move the stick until it is seen.
    let seen = |got: &[InputEvent]| {
        got.iter().any(|e| {
            matches!(e, InputEvent::PadAxis { axis: PadAxis::LeftX, value, .. }
                if (*value + 20000).abs() < 2)
        })
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let mut nudge = 0;
    while !seen(&got.lock().unwrap()) && std::time::Instant::now() < deadline {
        // The kernel drops unchanged values: alternate by one.
        nudge = 1 - nudge;
        u.inject(&InputEvent::PadAxis {
            pad: 0,
            axis: PadAxis::LeftX,
            value: -20000 + nudge,
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }
    // The reader sees changes only: release the button pressed before it
    // started, then press and release it again.
    for pressed in [false, true, false] {
        u.inject(&InputEvent::PadButton {
            pad: 0,
            code: 0x133,
            pressed,
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    drop(reader);
    let got = got.lock().unwrap().clone();
    assert!(seen(&got), "{got:?}");
    assert!(
        got.iter().any(|e| matches!(
            e,
            InputEvent::PadButton {
                code: 0x133,
                pressed: true,
                ..
            }
        )),
        "the top button stays the top button: {got:?}"
    );
    u.release_all();
}
