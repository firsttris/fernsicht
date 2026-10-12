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
    let reader = Gamepads::evdev("/dev/input".into(), false, move |e| {
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

/// The evdev node of the device the kernel calls `name`.
fn evdev_node(name: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir("/sys/class/input")
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
        .find(|e| {
            std::fs::read_to_string(e.path().join("device/name")).is_ok_and(|n| n.trim() == name)
        })
        .map(|e| std::path::Path::new("/dev/input").join(e.file_name()))
}

#[test]
fn rumble_a_game_plays_on_the_pad_goes_to_the_session() {
    use fernsicht_input::Rumble;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::sync::{Arc, Mutex, mpsc};
    if !enabled() {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut u = Uinput::open(AbsArea::default()).expect("open /dev/uinput");
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    u.on_rumble(Arc::new(move |r| {
        let _ = tx.lock().unwrap().send(r);
    }));
    u.inject(&InputEvent::PadButton {
        pad: 0,
        code: 0x130,
        pressed: false,
    })
    .unwrap();
    // As a game does: open the pad, upload a rumble effect, play it.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let pad = loop {
        let opened = evdev_node("Fernsicht X-Box 360 pad 1").and_then(|p| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(p)
                .ok()
        });
        match opened {
            Some(f) => break f,
            // udev sets the node's permissions a moment later.
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            None => panic!("the pad's evdev node never opened"),
        }
    };
    // struct ff_effect (linux/input.h): FF_RUMBLE, id -1 (a new one),
    // 300 ms, strong 0x8000, weak 0x2000.
    let mut effect = [0u8; 48];
    effect[0..2].copy_from_slice(&0x50u16.to_ne_bytes());
    effect[2..4].copy_from_slice(&(-1i16).to_ne_bytes());
    effect[10..12].copy_from_slice(&300u16.to_ne_bytes());
    effect[16..18].copy_from_slice(&0x8000u16.to_ne_bytes());
    effect[18..20].copy_from_slice(&0x2000u16.to_ne_bytes());
    // EVIOCSFF = _IOW('E', 0x80, struct ff_effect): returns once the
    // host's thread took the upload; the kernel writes the id back.
    // SAFETY: the evdev ioctl with a 48-byte struct ff_effect.
    let r = unsafe { libc::ioctl(pad.as_raw_fd(), 0x4030_4580, effect.as_mut_ptr()) };
    assert_eq!(r, 0, "upload: {}", std::io::Error::last_os_error());
    let id = u16::from_ne_bytes([effect[2], effect[3]]);
    let play = |value: i32| {
        let mut e = [0u8; 24];
        e[16..18].copy_from_slice(&0x15u16.to_ne_bytes());
        e[18..20].copy_from_slice(&id.to_ne_bytes());
        e[20..24].copy_from_slice(&value.to_ne_bytes());
        (&pad).write_all(&e).unwrap();
    };
    play(1);
    let got = rx.recv_timeout(Duration::from_secs(2)).expect("no rumble");
    assert_eq!(
        got,
        Rumble {
            pad: 0,
            low: 0x8000,
            high: 0x2000,
            ms: 300
        }
    );
    play(0);
    let got = rx.recv_timeout(Duration::from_secs(2)).expect("no stop");
    assert_eq!((got.low, got.high), (0, 0), "stopped");
    drop(pad);
    drop(u);
}

/// Reads `node`'s events until `want` picks one, for up to two seconds:
/// (type, code, value) of each `struct input_event`.
fn read_until(node: &std::fs::File, mut want: impl FnMut(u16, u16, i32) -> bool) -> bool {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 24 * 64];
    while std::time::Instant::now() < deadline {
        let mut fd = libc::pollfd {
            fd: node.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut fd, 1, 100) } <= 0 {
            continue;
        }
        let Ok(n) = (&*node).read(&mut buf) else {
            continue;
        };
        for e in buf[..n].as_chunks::<24>().0 {
            let kind = u16::from_ne_bytes([e[16], e[17]]);
            let code = u16::from_ne_bytes([e[18], e[19]]);
            let value = i32::from_ne_bytes([e[20], e[21], e[22], e[23]]);
            if want(kind, code, value) {
                return true;
            }
        }
    }
    false
}

/// Opens the evdev node the kernel calls `name`, once udev let us.
fn open_node(name: &str) -> Option<std::fs::File> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let opened = evdev_node(name).and_then(|p| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(p)
                .ok()
        });
        if opened.is_some() || std::time::Instant::now() >= deadline {
            return opened;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A client's PlayStation pad becomes a DualSense: the kernel's
/// hid-playstation driver binds to it (it asks for the calibration,
/// pairing and firmware reports), its gamepad, motion sensor and touchpad
/// devices show what was sent, and a game's rumble comes back. Needs
/// /dev/uhid and the driver (CI loads it; `FERNSICHT_REQUIRE_DUALSENSE`).
#[test]
fn a_dualsense_shows_buttons_motion_touch_and_rumbles() {
    use fernsicht_input::Rumble;
    use fernsicht_proto::{PadModel, PadSensor};
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::sync::{Arc, Mutex, mpsc};
    if !enabled() {
        return;
    }
    let driver = std::path::Path::new("/sys/bus/hid/drivers/playstation").exists();
    if !driver
        || std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uhid")
            .is_err()
    {
        assert!(
            std::env::var_os("FERNSICHT_REQUIRE_DUALSENSE").is_none(),
            "no hid-playstation or no /dev/uhid"
        );
        eprintln!("skipped: needs hid-playstation and a writable /dev/uhid");
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut u = Uinput::open(AbsArea::default()).expect("open /dev/uinput");
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    u.on_rumble(Arc::new(move |r| {
        let _ = tx.lock().unwrap().send(r);
    }));
    u.inject(&InputEvent::PadModel {
        pad: 1,
        model: PadModel::DualSense,
    })
    .unwrap();
    u.inject(&InputEvent::PadButton {
        pad: 1,
        code: 0x130,
        pressed: false,
    })
    .unwrap();
    let name = "Fernsicht DualSense pad 2";
    let pad = open_node(name).unwrap_or_else(|| panic!("no {name}:\n{}", devices()));
    let sensors = open_node(&format!("{name} Motion Sensors")).expect("no motion sensors");
    let touchpad = open_node(&format!("{name} Touchpad")).expect("no touchpad");
    assert!(!devices().contains("X-Box 360 pad 2"), "not an Xbox pad");

    let press = InputEvent::PadButton {
        pad: 1,
        code: 0x130,
        pressed: true,
    };
    u.inject(&press).unwrap();
    // EV_KEY BTN_SOUTH 1: the cross is the bottom button.
    assert!(
        read_until(&pad, |t, c, v| (t, c, v) == (1, 0x130, 1)),
        "cross"
    );
    u.inject(&InputEvent::PadMotion {
        pad: 1,
        sensor: PadSensor::Gyro,
        x: 16 * 90,
        y: 0,
        z: 0,
    })
    .unwrap();
    // EV_ABS ABS_RX in 1/1024 °/s: 90 °/s.
    assert!(
        read_until(&sensors, |t, c, v| (t, c, v) == (3, 0x03, 90 * 1024)),
        "gyro"
    );
    u.inject(&InputEvent::PadTouch {
        pad: 1,
        finger: 0,
        down: true,
        x: 65535,
        y: 0,
    })
    .unwrap();
    // EV_ABS ABS_MT_POSITION_X at the right edge.
    assert!(
        read_until(&touchpad, |t, c, v| (t, c, v) == (3, 0x35, 1919)),
        "touch"
    );

    // A game's rumble, as in the Xbox pad's test.
    let mut effect = [0u8; 48];
    effect[0..2].copy_from_slice(&0x50u16.to_ne_bytes());
    effect[2..4].copy_from_slice(&(-1i16).to_ne_bytes());
    effect[16..18].copy_from_slice(&0x8000u16.to_ne_bytes());
    effect[18..20].copy_from_slice(&0x2000u16.to_ne_bytes());
    // SAFETY: EVIOCSFF with a 48-byte struct ff_effect.
    let r = unsafe { libc::ioctl(pad.as_raw_fd(), 0x4030_4580, effect.as_mut_ptr()) };
    assert_eq!(r, 0, "upload: {}", std::io::Error::last_os_error());
    let id = u16::from_ne_bytes([effect[2], effect[3]]);
    let mut play = [0u8; 24];
    play[16..18].copy_from_slice(&0x15u16.to_ne_bytes());
    play[18..20].copy_from_slice(&id.to_ne_bytes());
    play[20..24].copy_from_slice(&1i32.to_ne_bytes());
    (&pad).write_all(&play).unwrap();
    // hid-playstation sends the motors' bytes (magnitude / 256).
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut got = None;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(r) if r.low != 0 || r.high != 0 => {
                got = Some(r);
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert_eq!(
        got,
        Some(Rumble {
            pad: 1,
            low: 0x80 * 257,
            high: 0x20 * 257,
            ms: 65_535
        })
    );
    // Released with the session: the cross goes up.
    u.release_all();
    assert!(
        read_until(&pad, |t, c, v| (t, c, v) == (1, 0x130, 0)),
        "released"
    );
    drop((pad, sensors, touchpad));
    drop(u);
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !devices().contains(name),
        "the DualSense goes with the sink"
    );
}
