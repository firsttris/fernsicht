//! Virtual input devices through `/dev/uinput`.
//!
//! Three devices, because libinput classifies a device by what it can do,
//! and a mix would confuse it:
//! - a keyboard (all Linux key codes),
//! - an absolute pointer (like a VM's "tablet": position 0..=65535 across
//!   the screen, buttons, wheel) for desktop use,
//! - a relative mouse (motion, buttons, wheel) for games,
//! - per client gamepad, created when it is first used: an Xbox 360 pad
//!   (its USB ids and the `xpad` driver's layout), which Steam, SDL and
//!   games know without configuration.
//!
//! Compositors spread an absolute pointer over the whole desktop (all
//! monitors). Positions arrive relative to the streamed monitor, so they
//! are mapped into that monitor's part of the desktop ([`AbsArea`]).
//!
//! Buttons and the wheel go to the pointer that moved last. Needs write
//! access to `/dev/uinput`: root, or the ACL desktops give the logged-in
//! user (Bazzite does, for Steam Input).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use fernsicht_proto::{BTN_MOUSE_FIRST, BTN_MOUSE_LAST, InputEvent, KEY_MAX, MAX_PADS, PadAxis};

use crate::InputSink;

// linux/input-event-codes.h
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BUS_VIRTUAL: u16 = 0x06;
const BUS_USB: u16 = 0x03;
const BTN_NORTH: u16 = 0x133;
const BTN_WEST: u16 = 0x134;
/// The buttons of an Xbox 360 pad: A B X Y, bumpers, back, start, guide,
/// stick clicks.
const PAD_BUTTONS: [u16; 11] = [
    0x130, 0x131, 0x133, 0x134, 0x136, 0x137, 0x13a, 0x13b, 0x13c, 0x13d, 0x13e,
];

// linux/uinput.h ioctls (x86_64 / aarch64 encoding).
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;

/// Pointer positions span 0..=ABS_MAX (the wire's 16-bit range).
pub const ABS_MAX: i32 = 65535;

#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// One `struct input_event` (64-bit time), as written to the device.
fn input_event(kind: u16, code: u16, value: i32) -> [u8; 24] {
    let mut e = [0u8; 24];
    // Time 0: the kernel stamps uinput events itself.
    e[16..18].copy_from_slice(&kind.to_ne_bytes());
    e[18..20].copy_from_slice(&code.to_ne_bytes());
    e[20..24].copy_from_slice(&value.to_ne_bytes());
    e
}

/// An absolute axis and its range.
#[derive(Clone, Copy)]
struct Abs {
    code: u16,
    min: i32,
    max: i32,
    flat: i32,
}

impl Abs {
    /// A pointer axis: 0..=[`ABS_MAX`].
    fn pointer(code: u16) -> Self {
        Self {
            code,
            min: 0,
            max: ABS_MAX,
            flat: 0,
        }
    }
}

/// What a device can do.
#[derive(Default)]
struct Caps {
    keys: Vec<u16>,
    rel: Vec<u16>,
    abs: Vec<Abs>,
}

/// How a device introduces itself.
struct Id {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

impl Id {
    /// Our own devices: not a registered vendor; stable ids for udev rules.
    fn own(product: u16) -> Self {
        Self {
            bustype: BUS_VIRTUAL,
            vendor: 0xf5f5,
            product,
            version: 1,
        }
    }
}

/// A created uinput device; destroyed on drop.
struct Device {
    file: File,
}

fn ioctl(file: &File, request: libc::c_ulong, arg: libc::c_ulong) -> Result<(), String> {
    // SAFETY: uinput ioctls on our descriptor; pointer arguments are passed
    // by the callers below and outlive the call.
    let r = unsafe { libc::ioctl(file.as_raw_fd(), request, arg) };
    if r < 0 {
        return Err(format!(
            "uinput ioctl {request:#x}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

impl Device {
    fn create(path: &Path, name: &str, id: Id, caps: &Caps) -> Result<Self, String> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        let set = |bit: libc::c_ulong, values: &[u16]| -> Result<(), String> {
            for &v in values {
                ioctl(&file, bit, libc::c_ulong::from(v))?;
            }
            Ok(())
        };
        set(UI_SET_EVBIT, &[EV_SYN])?;
        if !caps.keys.is_empty() {
            set(UI_SET_EVBIT, &[EV_KEY])?;
            set(UI_SET_KEYBIT, &caps.keys)?;
        }
        if !caps.rel.is_empty() {
            set(UI_SET_EVBIT, &[EV_REL])?;
            set(UI_SET_RELBIT, &caps.rel)?;
        }
        if !caps.abs.is_empty() {
            set(UI_SET_EVBIT, &[EV_ABS])?;
            let codes: Vec<u16> = caps.abs.iter().map(|a| a.code).collect();
            set(UI_SET_ABSBIT, &codes)?;
            for a in &caps.abs {
                let abs = UinputAbsSetup {
                    code: a.code,
                    _pad: 0,
                    value: 0,
                    minimum: a.min,
                    maximum: a.max,
                    fuzz: 0,
                    flat: a.flat,
                    resolution: 0,
                };
                ioctl(&file, UI_ABS_SETUP, &abs as *const _ as libc::c_ulong)?;
            }
        }
        let mut setup = UinputSetup {
            bustype: id.bustype,
            vendor: id.vendor,
            product: id.product,
            version: id.version,
            name: [0; 80],
            ff_effects_max: 0,
        };
        let n = name.len().min(79);
        setup.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        ioctl(&file, UI_DEV_SETUP, &setup as *const _ as libc::c_ulong)?;
        ioctl(&file, UI_DEV_CREATE, 0)?;
        Ok(Self { file })
    }

    /// Writes events followed by a SYN_REPORT, in one write.
    fn emit(&mut self, events: &[(u16, u16, i32)]) -> Result<(), String> {
        let mut buf = Vec::with_capacity((events.len() + 1) * 24);
        for &(kind, code, value) in events {
            buf.extend_from_slice(&input_event(kind, code, value));
        }
        buf.extend_from_slice(&input_event(EV_SYN, SYN_REPORT, 0));
        self.file
            .write_all(&buf)
            .map_err(|e| format!("uinput write: {e}"))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = ioctl(&self.file, UI_DEV_DESTROY, 0);
    }
}

/// Where the streamed monitor lies in the desktop, as fractions of the
/// desktop's bounding box (0..=1). The default is the whole desktop (one
/// monitor).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AbsArea {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Default for AbsArea {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        }
    }
}

impl AbsArea {
    /// A position on the streamed monitor (0..=65535 each way) in device
    /// units across the desktop.
    pub fn map(&self, x: u16, y: u16) -> (i32, i32) {
        let f = |offset: f64, size: f64, v: u16| {
            let t = offset + size * f64::from(v) / f64::from(u16::MAX);
            (t * f64::from(ABS_MAX))
                .round()
                .clamp(0.0, f64::from(ABS_MAX)) as i32
        };
        (f(self.x, self.width, x), f(self.y, self.height, y))
    }
}

/// Which pointer moved last (buttons and wheel go there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pointer {
    Absolute,
    Relative,
}

/// Keyboard, the two pointers and the gamepads.
pub struct Uinput {
    path: PathBuf,
    keyboard: Device,
    absolute: Device,
    relative: Device,
    pointer: Pointer,
    held: BTreeSet<u16>,
    area: AbsArea,
    pads: Vec<Option<Device>>,
    pad_held: BTreeSet<(u8, u16)>,
    pad_axes: BTreeMap<(u8, PadAxis), i32>,
}

/// A virtual Xbox 360 pad.
fn create_pad(path: &Path, n: u8) -> Result<Device, String> {
    let stick = |code| Abs {
        code,
        min: -32768,
        max: 32767,
        flat: 128,
    };
    let abs = PadAxis::ALL
        .iter()
        .map(|&a| {
            let r = a.range();
            match a {
                PadAxis::LeftX | PadAxis::LeftY | PadAxis::RightX | PadAxis::RightY => {
                    stick(a as u16)
                }
                _ => Abs {
                    code: a as u16,
                    min: *r.start(),
                    max: *r.end(),
                    flat: 0,
                },
            }
        })
        .collect();
    Device::create(
        path,
        &format!("Fernsicht X-Box 360 pad {}", n + 1),
        // Microsoft Xbox 360 controller, as the xpad driver reports it.
        Id {
            bustype: BUS_USB,
            vendor: 0x045e,
            product: 0x028e,
            version: 0x0110,
        },
        &Caps {
            keys: PAD_BUTTONS.to_vec(),
            rel: Vec::new(),
            abs,
        },
    )
}

/// The protocol's buttons are meant by position; an Xbox pad (xpad) has
/// X (left, `BTN_WEST` by position) as `BTN_X` = 0x133 and Y (top) as
/// `BTN_Y` = 0x134.
fn xpad_button(code: u16) -> u16 {
    match code {
        BTN_NORTH => BTN_WEST,
        BTN_WEST => BTN_NORTH,
        c => c,
    }
}

/// Every key code a keyboard may send (no mouse or joystick buttons, or
/// libinput would take the keyboard for something else).
fn keyboard_keys() -> Vec<u16> {
    (1..=KEY_MAX)
        .filter(|c| !(0x100..0x160).contains(c))
        .collect()
}

fn mouse_buttons() -> Vec<u16> {
    (BTN_MOUSE_FIRST..=BTN_MOUSE_LAST).collect()
}

const WHEELS: [u16; 4] = [REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES];

impl Uinput {
    /// `area`: where the streamed monitor lies in the desktop.
    pub fn open(area: AbsArea) -> Result<Self, String> {
        Self::open_at(Path::new("/dev/uinput"), area)
    }

    pub fn open_at(path: &Path, area: AbsArea) -> Result<Self, String> {
        let keyboard = Device::create(
            path,
            "Fernsicht keyboard",
            Id::own(1),
            &Caps {
                keys: keyboard_keys(),
                ..Caps::default()
            },
        )?;
        let absolute = Device::create(
            path,
            "Fernsicht pointer",
            Id::own(2),
            &Caps {
                keys: mouse_buttons(),
                rel: WHEELS.to_vec(),
                abs: vec![Abs::pointer(ABS_X), Abs::pointer(ABS_Y)],
            },
        )?;
        let relative = Device::create(
            path,
            "Fernsicht mouse",
            Id::own(3),
            &Caps {
                keys: mouse_buttons(),
                rel: [REL_X, REL_Y].into_iter().chain(WHEELS).collect(),
                abs: Vec::new(),
            },
        )?;
        Ok(Self {
            path: path.to_owned(),
            keyboard,
            absolute,
            relative,
            pointer: Pointer::Absolute,
            held: BTreeSet::new(),
            area,
            pads: (0..MAX_PADS).map(|_| None).collect(),
            pad_held: BTreeSet::new(),
            pad_axes: BTreeMap::new(),
        })
    }

    /// Gamepad `n`, created on first use.
    fn pad(&mut self, n: u8) -> Result<&mut Device, String> {
        let slot = self
            .pads
            .get_mut(usize::from(n))
            .ok_or_else(|| format!("no gamepad {n}"))?;
        if slot.is_none() {
            *slot = Some(create_pad(&self.path, n)?);
            log::info!("gamepad {} connected", n + 1);
        }
        Ok(slot.as_mut().expect("just created"))
    }

    fn pointer_device(&mut self) -> &mut Device {
        match self.pointer {
            Pointer::Absolute => &mut self.absolute,
            Pointer::Relative => &mut self.relative,
        }
    }
}

/// The evdev events for one input event, and the device they go to.
fn translate(event: &InputEvent, area: &AbsArea) -> (Target, Vec<(u16, u16, i32)>) {
    match *event {
        InputEvent::MouseAbs { x, y } => {
            let (x, y) = area.map(x, y);
            (
                Target::Absolute,
                vec![(EV_ABS, ABS_X, x), (EV_ABS, ABS_Y, y)],
            )
        }
        InputEvent::MouseRel { dx, dy } => (
            Target::Relative,
            vec![(EV_REL, REL_X, dx), (EV_REL, REL_Y, dy)],
        ),
        InputEvent::Button { code, pressed } => {
            (Target::Pointer, vec![(EV_KEY, code, i32::from(pressed))])
        }
        InputEvent::Scroll { dx, dy } => {
            let mut v = Vec::new();
            // Hi-res units plus the classic notches for older clients.
            if dy != 0 {
                v.push((EV_REL, REL_WHEEL_HI_RES, dy));
                if dy % 120 == 0 {
                    v.push((EV_REL, REL_WHEEL, dy / 120));
                }
            }
            if dx != 0 {
                v.push((EV_REL, REL_HWHEEL_HI_RES, dx));
                if dx % 120 == 0 {
                    v.push((EV_REL, REL_HWHEEL, dx / 120));
                }
            }
            (Target::Pointer, v)
        }
        InputEvent::Key { code, pressed } => {
            (Target::Keyboard, vec![(EV_KEY, code, i32::from(pressed))])
        }
        InputEvent::PadButton { pad, code, pressed } => (
            Target::Pad(pad),
            vec![(EV_KEY, xpad_button(code), i32::from(pressed))],
        ),
        InputEvent::PadAxis { pad, axis, value } => {
            (Target::Pad(pad), vec![(EV_ABS, axis as u16, value)])
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Keyboard,
    Absolute,
    Relative,
    /// Whichever pointer moved last.
    Pointer,
    Pad(u8),
}

impl InputSink for Uinput {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        match *event {
            InputEvent::Key { code, pressed } | InputEvent::Button { code, pressed } => {
                if pressed {
                    self.held.insert(code);
                } else {
                    self.held.remove(&code);
                }
            }
            InputEvent::PadButton { pad, code, pressed } => {
                if pressed {
                    self.pad_held.insert((pad, code));
                } else {
                    self.pad_held.remove(&(pad, code));
                }
            }
            InputEvent::PadAxis { pad, axis, value } => {
                if value == 0 {
                    self.pad_axes.remove(&(pad, axis));
                } else {
                    self.pad_axes.insert((pad, axis), value);
                }
            }
            _ => {}
        }
        let (target, events) = translate(event, &self.area);
        if events.is_empty() {
            return Ok(());
        }
        let device = match target {
            Target::Keyboard => &mut self.keyboard,
            Target::Absolute => {
                self.pointer = Pointer::Absolute;
                &mut self.absolute
            }
            Target::Relative => {
                self.pointer = Pointer::Relative;
                &mut self.relative
            }
            Target::Pointer => self.pointer_device(),
            Target::Pad(n) => self.pad(n)?,
        };
        device.emit(&events)
    }

    fn release_all(&mut self) {
        for code in std::mem::take(&mut self.held) {
            let up = [(EV_KEY, code, 0)];
            let device = if (BTN_MOUSE_FIRST..=BTN_MOUSE_LAST).contains(&code) {
                self.pointer_device()
            } else {
                &mut self.keyboard
            };
            if let Err(e) = device.emit(&up) {
                log::warn!("releasing {code:#x}: {e}");
            }
        }
        // Gamepads: buttons up, sticks and triggers at rest.
        let buttons = std::mem::take(&mut self.pad_held)
            .into_iter()
            .map(|(pad, code)| (pad, (EV_KEY, xpad_button(code), 0)));
        let axes = std::mem::take(&mut self.pad_axes)
            .into_keys()
            .map(|(pad, axis)| (pad, (EV_ABS, axis as u16, 0)));
        for (pad, event) in buttons.chain(axes).collect::<Vec<_>>() {
            if let Some(Some(d)) = self.pads.get_mut(usize::from(pad))
                && let Err(e) = d.emit(&[event])
            {
                log::warn!("releasing gamepad {}: {e}", pad + 1);
            }
        }
    }
}

impl Drop for Uinput {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_layout_matches_struct_input_event() {
        let e = input_event(EV_KEY, 30, 1);
        assert_eq!(&e[..16], &[0; 16], "time left to the kernel");
        assert_eq!(&e[16..18], &EV_KEY.to_ne_bytes());
        assert_eq!(&e[18..20], &30u16.to_ne_bytes());
        assert_eq!(&e[20..24], &1i32.to_ne_bytes());
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
    }

    fn translate_whole(e: &InputEvent) -> (Target, Vec<(u16, u16, i32)>) {
        translate(e, &AbsArea::default())
    }

    #[test]
    fn positions_land_on_the_streamed_monitor() {
        // Right half of a two-monitor desktop (zentrale: DP-1 at 2560 of
        // 5120): the stream's left edge is the desktop's middle.
        let right = AbsArea {
            x: 0.5,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        };
        assert_eq!(right.map(0, 0), (ABS_MAX / 2 + 1, 0));
        assert_eq!(right.map(u16::MAX, u16::MAX), (ABS_MAX, ABS_MAX));
        assert_eq!(right.map(32768, 32768).0, 49152);
        // One monitor: unchanged.
        assert_eq!(AbsArea::default().map(1234, 4321), (1234, 4321));
        // Out-of-range areas cannot leave the device range.
        let odd = AbsArea {
            x: 0.9,
            y: -0.5,
            width: 0.5,
            height: 0.5,
        };
        assert_eq!(odd.map(u16::MAX, 0), (ABS_MAX, 0));
    }

    #[test]
    fn events_go_to_the_right_device() {
        let (t, e) = translate_whole(&InputEvent::MouseAbs { x: 100, y: 65535 });
        assert_eq!(t, Target::Absolute);
        assert_eq!(e, vec![(EV_ABS, ABS_X, 100), (EV_ABS, ABS_Y, 65535)]);
        let (t, _) = translate_whole(&InputEvent::MouseRel { dx: -1, dy: 2 });
        assert_eq!(t, Target::Relative);
        let (t, e) = translate_whole(&InputEvent::Key {
            code: 30,
            pressed: true,
        });
        assert_eq!((t, e), (Target::Keyboard, vec![(EV_KEY, 30, 1)]));
        let (t, e) = translate_whole(&InputEvent::Button {
            code: 0x111,
            pressed: false,
        });
        assert_eq!((t, e), (Target::Pointer, vec![(EV_KEY, 0x111, 0)]));
    }

    #[test]
    fn wheel_sends_hi_res_and_whole_notches() {
        let (_, e) = translate_whole(&InputEvent::Scroll { dx: 0, dy: -240 });
        assert_eq!(
            e,
            vec![(EV_REL, REL_WHEEL_HI_RES, -240), (EV_REL, REL_WHEEL, -2)]
        );
        // A touchpad's fraction of a notch: hi-res only.
        let (_, e) = translate_whole(&InputEvent::Scroll { dx: 30, dy: 0 });
        assert_eq!(e, vec![(EV_REL, REL_HWHEEL_HI_RES, 30)]);
        let (_, e) = translate_whole(&InputEvent::Scroll { dx: 0, dy: 0 });
        assert!(e.is_empty());
    }

    #[test]
    fn gamepads_look_like_xbox_pads() {
        // A by position is A; top (Y) and left (X) swap into xpad's codes.
        let (t, e) = translate_whole(&InputEvent::PadButton {
            pad: 2,
            code: 0x130,
            pressed: true,
        });
        assert_eq!((t, e), (Target::Pad(2), vec![(EV_KEY, 0x130, 1)]));
        let (_, e) = translate_whole(&InputEvent::PadButton {
            pad: 0,
            code: BTN_NORTH,
            pressed: false,
        });
        assert_eq!(e, vec![(EV_KEY, BTN_WEST, 0)]);
        assert_eq!(xpad_button(BTN_WEST), BTN_NORTH);
        let (t, e) = translate_whole(&InputEvent::PadAxis {
            pad: 1,
            axis: PadAxis::RightTrigger,
            value: 200,
        });
        assert_eq!((t, e), (Target::Pad(1), vec![(EV_ABS, 0x05, 200)]));
    }

    #[test]
    fn keyboard_has_no_mouse_buttons() {
        let keys = keyboard_keys();
        assert!(keys.contains(&30) && keys.contains(&KEY_MAX));
        assert!(!keys.iter().any(|k| (0x100..0x160).contains(k)));
    }

    #[test]
    fn missing_device_node_is_a_clear_error() {
        let e = Uinput::open_at(Path::new("/does/not/exist"), AbsArea::default())
            .err()
            .unwrap();
        assert!(e.contains("/does/not/exist"), "{e}");
    }
}
