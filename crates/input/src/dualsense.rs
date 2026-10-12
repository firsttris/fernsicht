//! A DualSense as the host's kernel and games see it (D9): the reports of
//! Sony's pad over USB, built from the protocol's events. Pure; the device
//! itself is [`crate::uhid`]. The layouts follow the kernel's
//! `drivers/hid/hid-playstation.c` (`dualsense_input_report`,
//! `dualsense_output_report_common`, the feature reports it reads at
//! probe) and SDL's `SDL_hidapi_ps5.c`, which reads the same reports
//! through hidraw (Steam does too); the report descriptor is the pad's
//! own (nondebug/dualsense `report-descriptor-usb.txt`, as inputtino
//! carries it).

use fernsicht_proto::{BTN_PAD_TOUCHPAD, InputEvent, MAX_TOUCH_FINGERS, PadAxis, PadSensor};

use crate::Rumble;

/// Sony, DualSense (`USB_VENDOR_ID_SONY`, `USB_DEVICE_ID_SONY_PS5_CONTROLLER`).
pub const VENDOR: u32 = 0x054c;
pub const PRODUCT: u32 = 0x0ce6;

/// The DualSense's USB report descriptor (273 bytes).
pub const DESCRIPTOR: [u8; 273] = [
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x06,
    0x00, 0xff, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07,
    0x35, 0x00, 0x46, 0x3b, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05,
    0x09, 0x19, 0x01, 0x29, 0x0f, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0f, 0x81, 0x02, 0x06,
    0x00, 0xff, 0x09, 0x21, 0x95, 0x0d, 0x81, 0x02, 0x06, 0x00, 0xff, 0x09, 0x22, 0x15, 0x00, 0x26,
    0xff, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02, 0x85, 0x02, 0x09, 0x23, 0x95, 0x2f, 0x91, 0x02,
    0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xb1, 0x02, 0x85, 0x08, 0x09, 0x34, 0x95, 0x2f, 0xb1, 0x02,
    0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xb1, 0x02, 0x85, 0x0a, 0x09, 0x25, 0x95, 0x1a, 0xb1, 0x02,
    0x85, 0x20, 0x09, 0x26, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x21, 0x09, 0x27, 0x95, 0x04, 0xb1, 0x02,
    0x85, 0x22, 0x09, 0x40, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x80, 0x09, 0x28, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0x81, 0x09, 0x29, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x82, 0x09, 0x2a, 0x95, 0x09, 0xb1, 0x02,
    0x85, 0x83, 0x09, 0x2b, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0x84, 0x09, 0x2c, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0x85, 0x09, 0x2d, 0x95, 0x02, 0xb1, 0x02, 0x85, 0xa0, 0x09, 0x2e, 0x95, 0x01, 0xb1, 0x02,
    0x85, 0xe0, 0x09, 0x2f, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf0, 0x09, 0x30, 0x95, 0x3f, 0xb1, 0x02,
    0x85, 0xf1, 0x09, 0x31, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf2, 0x09, 0x32, 0x95, 0x0f, 0xb1, 0x02,
    0x85, 0xf4, 0x09, 0x35, 0x95, 0x3f, 0xb1, 0x02, 0x85, 0xf5, 0x09, 0x36, 0x95, 0x03, 0xb1, 0x02,
    0xc0,
];

/// The input report: id 0x01 and 63 bytes (`DS_INPUT_REPORT_USB_SIZE`).
pub const INPUT_LEN: usize = 64;
const INPUT_REPORT: u8 = 0x01;
const OUTPUT_REPORT: u8 = 0x02;
const CALIBRATION: u8 = 0x05;
const PAIRING_INFO: u8 = 0x09;
const FIRMWARE_INFO: u8 = 0x20;

/// The touchpad's size in its own units (`DS_TOUCHPAD_WIDTH`, `_HEIGHT`).
const TOUCH_WIDTH: u32 = 1920;
const TOUCH_HEIGHT: u32 = 1080;

// Buttons in the report's bytes 8..11 (hid-playstation.c `DS_BUTTONS*`).
const SQUARE: (usize, u8) = (8, 0x10);
const CROSS: (usize, u8) = (8, 0x20);
const CIRCLE: (usize, u8) = (8, 0x40);
const TRIANGLE: (usize, u8) = (8, 0x80);
const L1: (usize, u8) = (9, 0x01);
const R1: (usize, u8) = (9, 0x02);
const L2: (usize, u8) = (9, 0x04);
const R2: (usize, u8) = (9, 0x08);
const CREATE: (usize, u8) = (9, 0x10);
const OPTIONS: (usize, u8) = (9, 0x20);
const L3: (usize, u8) = (9, 0x40);
const R3: (usize, u8) = (9, 0x80);
const PS: (usize, u8) = (10, 0x01);
const TOUCHPAD: (usize, u8) = (10, 0x02);

/// Where a protocol button (meant by place) is on a DualSense.
fn button(code: u16) -> Option<(usize, u8)> {
    Some(match code {
        0x130 => CROSS,    // BTN_SOUTH
        0x131 => CIRCLE,   // BTN_EAST
        0x133 => TRIANGLE, // BTN_NORTH
        0x134 => SQUARE,   // BTN_WEST
        0x136 => L1,       // BTN_TL
        0x137 => R1,       // BTN_TR
        0x138 => L2,       // BTN_TL2
        0x139 => R2,       // BTN_TR2
        0x13a => CREATE,   // BTN_SELECT
        0x13b => OPTIONS,  // BTN_START
        0x13c => PS,       // BTN_MODE
        0x13d => L3,       // BTN_THUMBL
        0x13e => R3,       // BTN_THUMBR
        BTN_PAD_TOUCHPAD => TOUCHPAD,
        _ => return None,
    })
}

/// A finger on the touchpad.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Finger {
    down: bool,
    /// Counts up with every new touch (7 bits), as the pad's does.
    id: u8,
    x: u16,
    y: u16,
}

/// Everything a DualSense reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    /// Left x, y, right x, y: 0..=255, 0x80 at rest, 0 left / up.
    sticks: [u8; 4],
    triggers: [u8; 2],
    /// Button bits of bytes 8..11 (the hat's nibble aside).
    buttons: [u8; 3],
    /// The D-pad: x and y, -1..=1.
    dpad: (i8, i8),
    gyro: [i16; 3],
    accel: [i16; 3],
    fingers: [Finger; MAX_TOUCH_FINGERS as usize],
    touches: u8,
}

impl Default for State {
    fn default() -> Self {
        Self {
            sticks: [0x80; 4],
            triggers: [0; 2],
            buttons: [0; 3],
            dpad: (0, 0),
            gyro: [0; 3],
            accel: [0; 3],
            fingers: Default::default(),
            touches: 0,
        }
    }
}

/// A stick's protocol value (-32768..=32767) as the report's byte.
fn stick(value: i32) -> u8 {
    ((value.clamp(-32768, 32767) + 32768) >> 8) as u8
}

/// The hat switch's value for a D-pad position: 0 up, clockwise to 7,
/// 8 at rest.
fn hat((x, y): (i8, i8)) -> u8 {
    match (x, y) {
        (0, -1) => 0,
        (1, -1) => 1,
        (1, 0) => 2,
        (1, 1) => 3,
        (0, 1) => 4,
        (-1, 1) => 5,
        (-1, 0) => 6,
        (-1, -1) => 7,
        _ => 8,
    }
}

impl State {
    /// Takes one of the pad's events; whether the report changed.
    pub fn apply(&mut self, event: &InputEvent) -> bool {
        let before = self.clone();
        match *event {
            InputEvent::PadButton { code, pressed, .. } => {
                if let Some((byte, bit)) = button(code) {
                    let b = &mut self.buttons[byte - 8];
                    *b = if pressed { *b | bit } else { *b & !bit };
                }
            }
            InputEvent::PadAxis { axis, value, .. } => match axis {
                PadAxis::LeftX => self.sticks[0] = stick(value),
                PadAxis::LeftY => self.sticks[1] = stick(value),
                PadAxis::RightX => self.sticks[2] = stick(value),
                PadAxis::RightY => self.sticks[3] = stick(value),
                PadAxis::LeftTrigger => self.triggers[0] = value.clamp(0, 255) as u8,
                PadAxis::RightTrigger => self.triggers[1] = value.clamp(0, 255) as u8,
                PadAxis::DpadX => self.dpad.0 = value.clamp(-1, 1) as i8,
                PadAxis::DpadY => self.dpad.1 = value.clamp(-1, 1) as i8,
            },
            InputEvent::PadMotion {
                sensor, x, y, z, ..
            } => match sensor {
                PadSensor::Gyro => self.gyro = [x, y, z],
                PadSensor::Accel => self.accel = [x, y, z],
            },
            InputEvent::PadTouch {
                finger, down, x, y, ..
            } => {
                if let Some(f) = self.fingers.get_mut(usize::from(finger)) {
                    if down && !f.down {
                        self.touches = (self.touches + 1) & 0x7f;
                        f.id = self.touches;
                    }
                    f.down = down;
                    if down {
                        f.x = (u32::from(x) * (TOUCH_WIDTH - 1) / 65535) as u16;
                        f.y = (u32::from(y) * (TOUCH_HEIGHT - 1) / 65535) as u16;
                    }
                }
            }
            _ => {}
        }
        *self != before
    }

    /// The pad at rest: buttons up, sticks centred, no finger down
    /// (motion stays what it was: the pad still lies where it lay).
    pub fn released(&self) -> Self {
        Self {
            gyro: self.gyro,
            accel: self.accel,
            touches: self.touches,
            ..Self::default()
        }
    }
}

/// The input report for `state`; `seq` counts reports, `timestamp` is the
/// sensors' clock in 1/3 µs (wraps).
pub fn input_report(state: &State, seq: u8, timestamp: u32) -> [u8; INPUT_LEN] {
    let mut r = [0u8; INPUT_LEN];
    r[0] = INPUT_REPORT;
    r[1..5].copy_from_slice(&state.sticks);
    r[5..7].copy_from_slice(&state.triggers);
    r[7] = seq;
    r[8] = hat(state.dpad) | state.buttons[0];
    // The triggers' digital bits follow how far they are pulled, as on
    // the pad (and as L2/R2 buttons set them).
    r[9] = state.buttons[1]
        | if state.triggers[0] > 0 { L2.1 } else { 0 }
        | if state.triggers[1] > 0 { R2.1 } else { 0 };
    r[10] = state.buttons[2];
    for (i, v) in state.gyro.iter().chain(&state.accel).enumerate() {
        r[16 + 2 * i..18 + 2 * i].copy_from_slice(&v.to_le_bytes());
    }
    r[28..32].copy_from_slice(&timestamp.to_le_bytes());
    for (i, f) in state.fingers.iter().enumerate() {
        let p = &mut r[33 + 4 * i..37 + 4 * i];
        // The high bit set: no contact.
        p[0] = f.id & 0x7f | if f.down { 0 } else { 0x80 };
        p[1] = f.x as u8;
        p[2] = (f.x >> 8) as u8 & 0x0f | (f.y as u8 & 0x0f) << 4;
        p[3] = (f.y >> 4) as u8;
    }
    // Battery full, not charging (`DS_STATUS0_*`): no low-battery warnings.
    r[53] = 0x2a;
    r
}

/// The calibration the pad reports (feature report 0x05, 41 bytes): no
/// bias, ±8000 for ±1000 °/s and ±8192 for ±1 g, so a report's raw values
/// are the protocol's units (1/16 °/s, 1/8192 g), the nominal resolution
/// the kernel and SDL scale to. SDL ignores a calibration more than 50 %
/// off that (`HIDAPI_DriverPS5_LoadCalibrationData`).
fn calibration() -> Vec<u8> {
    let mut r = vec![0u8; 41];
    r[0] = CALIBRATION;
    let gyro: [i16; 8] = [8000, -8000, 8000, -8000, 8000, -8000, 500, 500];
    let accel: [i16; 6] = [8192, -8192, 8192, -8192, 8192, -8192];
    for (i, v) in gyro.iter().chain(&accel).enumerate() {
        r[7 + 2 * i..9 + 2 * i].copy_from_slice(&v.to_le_bytes());
    }
    r
}

/// Firmware info (feature report 0x20, 64 bytes): a real pad's, from
/// inputtino. The kernel reads the hardware and firmware versions at 24
/// and 28 and the update version (0x0154: classic rumble flags) at 44.
const FIRMWARE: [u8; 64] = [
    0x20, 0x4a, 0x75, 0x6e, 0x20, 0x31, 0x39, 0x20, 0x32, 0x30, 0x32, 0x33, 0x31, 0x34, 0x3a, 0x34,
    0x37, 0x3a, 0x33, 0x34, 0x03, 0x00, 0x44, 0x00, 0x08, 0x02, 0x00, 0x01, 0x36, 0x00, 0x00, 0x01,
    0xc1, 0xc8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x54, 0x01, 0x00, 0x00,
    0x14, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The answer to a feature report request, or `None` for one the pad does
/// not answer. `mac` is the pad's address (pairing info, 20 bytes: the
/// kernel takes bytes 1..7 for the device's `uniq`).
pub fn feature(report: u8, mac: [u8; 6]) -> Option<Vec<u8>> {
    match report {
        CALIBRATION => Some(calibration()),
        PAIRING_INFO => {
            let mut r = vec![0u8; 20];
            r[0] = PAIRING_INFO;
            r[1..7].copy_from_slice(&mac);
            Some(r)
        }
        FIRMWARE_INFO => Some(FIRMWARE.to_vec()),
        _ => None,
    }
}

/// The rumble an output report asks pad `pad` for, if it asks for one. The
/// kernel sets `COMPATIBLE_VIBRATION` in flag 0 or `COMPATIBLE_VIBRATION2`
/// in flag 2, SDL the first; the right motor is the light one. A rumble
/// lasts until the next.
pub fn rumble(pad: u8, output: &[u8]) -> Option<Rumble> {
    // id, flag 0, flag 1, right motor, left motor, ..., flag 2 at 1 + 38.
    if output.len() < 40 || output[0] != OUTPUT_REPORT {
        return None;
    }
    let vibrate = output[1] & 0x01 != 0 || output[39] & 0x04 != 0;
    vibrate.then(|| {
        let (right, left) = (output[3], output[4]);
        let on = right > 0 || left > 0;
        Rumble {
            pad,
            low: u16::from(left) * 257,
            high: u16::from(right) * 257,
            ms: if on { u32::from(u16::MAX) } else { 0 },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis(axis: PadAxis, value: i32) -> InputEvent {
        InputEvent::PadAxis {
            pad: 0,
            axis,
            value,
        }
    }

    fn press(code: u16, pressed: bool) -> InputEvent {
        InputEvent::PadButton {
            pad: 0,
            code,
            pressed,
        }
    }

    #[test]
    fn a_pad_at_rest_reports_rest() {
        let r = input_report(&State::default(), 7, 0x1234_5678);
        assert_eq!(r[0], 0x01);
        assert_eq!(r[1..5], [0x80; 4], "sticks centred");
        assert_eq!(r[5..7], [0, 0], "triggers released");
        assert_eq!(r[7], 7);
        assert_eq!(r[8], 8, "hat at rest, no face button");
        assert_eq!(r[9..11], [0, 0]);
        assert_eq!(r[28..32], 0x1234_5678u32.to_le_bytes());
        assert_eq!((r[33] & 0x80, r[37] & 0x80), (0x80, 0x80), "no finger");
        assert_eq!(r[53], 0x2a, "battery full");
    }

    #[test]
    fn buttons_and_sticks_land_where_the_kernel_reads_them() {
        let mut s = State::default();
        assert!(s.apply(&press(0x130, true)));
        assert!(!s.apply(&press(0x130, true)), "no change");
        for e in [
            press(0x133, true),
            press(0x13c, true),
            press(BTN_PAD_TOUCHPAD, true),
            press(0x13b, true),
            axis(PadAxis::LeftX, -32768),
            axis(PadAxis::LeftY, 32767),
            axis(PadAxis::RightX, 0),
            axis(PadAxis::RightTrigger, 255),
            axis(PadAxis::DpadX, 1),
            axis(PadAxis::DpadY, -1),
        ] {
            s.apply(&e);
        }
        let r = input_report(&s, 0, 0);
        assert_eq!(r[1..5], [0x00, 0xff, 0x80, 0x80]);
        assert_eq!(r[5..7], [0, 255]);
        assert_eq!(r[8], 0x20 | 0x80 | 1, "cross, triangle, up-right");
        assert_eq!(r[9], 0x20 | 0x08, "options, R2 pulled");
        assert_eq!(r[10], 0x01 | 0x02, "PS, touchpad click");
        s.apply(&press(0x130, false));
        assert_eq!(input_report(&s, 0, 0)[8] & 0x20, 0, "cross up again");
        assert_eq!(
            (0..9)
                .map(|i| hat([
                    (0, -1),
                    (1, -1),
                    (1, 0),
                    (1, 1),
                    (0, 1),
                    (-1, 1),
                    (-1, 0),
                    (-1, -1),
                    (0, 0)
                ][i]))
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        let rest = s.released();
        assert_eq!(
            input_report(&rest, 0, 0)[1..11],
            input_report(&State::default(), 0, 0)[1..11]
        );
    }

    #[test]
    fn motion_and_touch_are_in_the_pads_units() {
        let mut s = State::default();
        s.apply(&InputEvent::PadMotion {
            pad: 0,
            sensor: PadSensor::Gyro,
            x: 16 * 90,
            y: -1,
            z: i16::MIN,
        });
        s.apply(&InputEvent::PadMotion {
            pad: 0,
            sensor: PadSensor::Accel,
            x: 0,
            y: 8192,
            z: -8192,
        });
        let touch = |finger, down, x, y| InputEvent::PadTouch {
            pad: 0,
            finger,
            down,
            x,
            y,
        };
        s.apply(&touch(0, true, 65535, 65535));
        s.apply(&touch(1, true, 0, 32768));
        let r = input_report(&s, 0, 0);
        let le = |at: usize| i16::from_le_bytes([r[at], r[at + 1]]);
        assert_eq!([le(16), le(18), le(20)], [1440, -1, i16::MIN]);
        assert_eq!([le(22), le(24), le(26)], [0, 8192, -8192]);
        // As hid-playstation.c reads a touch point.
        let point = |at: usize| {
            let p = &r[at..at + 4];
            let x = u16::from(p[1]) | u16::from(p[2] & 0x0f) << 8;
            let y = u16::from(p[2] >> 4) | u16::from(p[3]) << 4;
            (p[0] & 0x80 == 0, p[0] & 0x7f, x, y)
        };
        assert_eq!(point(33), (true, 1, 1919, 1079));
        assert_eq!(point(37), (true, 2, 0, 539));
        // Moving keeps the touch's id, lifting and touching again counts on.
        s.apply(&touch(0, true, 0, 0));
        assert_eq!(point_of(&s, 33), (true, 1, 0, 0));
        s.apply(&touch(0, false, 0, 0));
        assert!(!point_of(&s, 33).0);
        s.apply(&touch(0, true, 0, 0));
        assert_eq!(point_of(&s, 33).1, 3);
    }

    fn point_of(s: &State, at: usize) -> (bool, u8, u16, u16) {
        let r = input_report(s, 0, 0);
        let p = &r[at..at + 4];
        (
            p[0] & 0x80 == 0,
            p[0] & 0x7f,
            u16::from(p[1]) | u16::from(p[2] & 0x0f) << 8,
            u16::from(p[2] >> 4) | u16::from(p[3]) << 4,
        )
    }

    #[test]
    fn the_calibration_makes_raw_values_the_protocols_units() {
        let c = feature(0x05, [0; 6]).unwrap();
        assert_eq!(c.len(), 41);
        let le = |at: usize| i32::from(i16::from_le_bytes([c[at], c[at + 1]]));
        // hid-playstation.c dualsense_get_calibration_data: evdev units
        // are 1/1024 °/s and 1/8192 g.
        let speed_2x = le(19) + le(21);
        let gyro_denom = (le(7) - le(1)).abs() + (le(9) - le(1)).abs();
        assert_eq!(speed_2x * 1024 / gyro_denom, 64, "1/16 °/s per unit");
        let range_2g = le(23) - le(25);
        assert_eq!(le(23) - range_2g / 2, 0, "no accelerometer bias");
        assert_eq!(2 * 8192 / range_2g, 1, "1/8192 g per unit");
        // SDL's check: sensitivity within 50 % of 64 and 1.
        assert_eq!(
            (
                speed_2x as f32 * 1024.0 / (le(7) - le(9)) as f32,
                2.0 * 8192.0 / range_2g as f32
            ),
            (64.0, 1.0)
        );
    }

    #[test]
    fn feature_reports_have_the_sizes_the_kernel_checks() {
        let mac = [1, 2, 3, 4, 5, 6];
        let pairing = feature(0x09, mac).unwrap();
        assert_eq!(
            (pairing.len(), pairing[0], &pairing[1..7]),
            (20, 0x09, &mac[..])
        );
        let fw = feature(0x20, mac).unwrap();
        assert_eq!((fw.len(), fw[0]), (64, 0x20));
        assert_eq!(u16::from_le_bytes([fw[44], fw[45]]), 0x0154);
        assert_eq!(feature(0x08, mac), None);
        assert_eq!(DESCRIPTOR.len(), 273);
        assert_eq!(
            &DESCRIPTOR[..8],
            &[0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01]
        );
    }

    #[test]
    fn rumble_is_read_from_the_output_report() {
        let mut out = [0u8; 48];
        out[0] = 0x02;
        assert_eq!(rumble(1, &out), None, "lightbar only, say");
        out[1] = 0x01 | 0x02;
        out[3] = 0x80;
        out[4] = 0xff;
        assert_eq!(
            rumble(1, &out),
            Some(Rumble {
                pad: 1,
                low: 0xffff,
                high: 0x8080,
                ms: 65_535
            })
        );
        // The kernel's newer flag; both motors off is a stop.
        let mut stop = [0u8; 48];
        stop[0] = 0x02;
        stop[39] = 0x04;
        assert_eq!(
            rumble(0, &stop).map(|r| (r.low, r.high, r.ms)),
            Some((0, 0, 0))
        );
        assert_eq!(rumble(0, &stop[..20]), None, "short");
        let mut bt = out;
        bt[0] = 0x31;
        assert_eq!(rumble(0, &bt), None, "not the USB report");
    }
}
