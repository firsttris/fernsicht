//! What the browser sends over the data channel: input as JSON.

use fernsicht_proto::{BTN_PAD_FIRST, BTN_PAD_LAST, InputEvent, MAX_PADS, PadAxis};
use serde_json::Value;

/// What the browser sends over the data channel.
#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Event(InputEvent),
    /// A numbered pointer position: applied only if newer than the last.
    Position(InputEvent, u32),
    /// Let go of everything (the page lost focus).
    ReleaseAll,
    Keyframe,
    /// Show another monitor (index in the list the browser got).
    Monitor(u8),
    /// The browser ends the session.
    Bye,
}

/// Parses one input message:
///
/// ```text
/// {"t":"m","x":0..65535,"y":0..65535,"s":n}  pointer at a position (n numbers them)
/// {"t":"r","dx":..,"dy":..}             pointer moved (captured)
/// {"t":"b","c":272,"p":true}            button (Linux BTN_* code)
/// {"t":"w","dx":0,"dy":120}             wheel, 120 per notch
/// {"t":"k","c":30,"p":true}             key (Linux KEY_* code)
/// {"t":"pb","n":0,"c":304,"p":true}     gamepad n's button (BTN_SOUTH..)
/// {"t":"pa","n":0,"a":0,"v":-32768}     gamepad n's axis (ABS_* code)
/// {"t":"release"}  {"t":"keyframe"}  {"t":"bye"}
/// ```
///
/// Anything out of range is refused, as in the native protocol.
pub(crate) fn parse_input(data: &[u8]) -> Option<Command> {
    let v: Value = serde_json::from_slice(data).ok()?;
    let int = |k: &str, lo: i64, hi: i64| v.get(k)?.as_i64().filter(|n| (lo..=hi).contains(n));
    let pressed = || v.get("p")?.as_bool();
    const MOVE: i64 = 1 << 16;
    const WHEEL: i64 = 120 * 100;
    let event = match v.get("t")?.as_str()? {
        "m" => {
            let at = InputEvent::MouseAbs {
                x: int("x", 0, 65535)? as u16,
                y: int("y", 0, 65535)? as u16,
            };
            return Some(match v.get("s") {
                Some(_) => Command::Position(at, int("s", 0, i64::from(u32::MAX))? as u32),
                None => Command::Event(at),
            });
        }
        "r" => InputEvent::MouseRel {
            dx: int("dx", -MOVE, MOVE)? as i32,
            dy: int("dy", -MOVE, MOVE)? as i32,
        },
        "b" => InputEvent::Button {
            code: int("c", 0x110, 0x117)? as u16,
            pressed: pressed()?,
        },
        "w" => InputEvent::Scroll {
            dx: int("dx", -WHEEL, WHEEL)? as i32,
            dy: int("dy", -WHEEL, WHEEL)? as i32,
        },
        "k" => InputEvent::Key {
            code: int("c", 1, 0x2ff)? as u16,
            pressed: pressed()?,
        },
        "pb" => InputEvent::PadButton {
            pad: int("n", 0, i64::from(MAX_PADS) - 1)? as u8,
            code: int("c", i64::from(BTN_PAD_FIRST), i64::from(BTN_PAD_LAST))? as u16,
            pressed: pressed()?,
        },
        "pa" => {
            let axis = PadAxis::from_code(int("a", 0, 0xff)? as u16)?;
            let r = axis.range();
            InputEvent::PadAxis {
                pad: int("n", 0, i64::from(MAX_PADS) - 1)? as u8,
                axis,
                value: int("v", i64::from(*r.start()), i64::from(*r.end()))? as i32,
            }
        }
        "release" => return Some(Command::ReleaseAll),
        "keyframe" => return Some(Command::Keyframe),
        "bye" => return Some(Command::Bye),
        "monitor" => {
            let max = fernsicht_proto::MAX_MONITORS as i64 - 1;
            return Some(Command::Monitor(int("i", 0, max)? as u8));
        }
        _ => return None,
    };
    Some(Command::Event(event))
}

/// Whether position number `a` came after `b` (they wrap around).
pub(super) fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_numbered_and_old_ones_known() {
        let p = |s: &str| parse_input(s.as_bytes());
        let at = InputEvent::MouseAbs { x: 1, y: 2 };
        assert_eq!(
            p(r#"{"t":"m","x":1,"y":2,"s":7}"#),
            Some(Command::Position(at, 7))
        );
        assert_eq!(
            p(r#"{"t":"m","x":1,"y":2,"s":4294967295}"#),
            Some(Command::Position(at, u32::MAX))
        );
        assert_eq!(p(r#"{"t":"m","x":1,"y":2,"s":-1}"#), None);
        assert_eq!(p(r#"{"t":"m","x":1,"y":2,"s":"7"}"#), None);
        assert!(newer(8, 7) && !newer(7, 7) && !newer(6, 7));
        assert!(newer(0, u32::MAX), "numbers wrap");
    }

    #[test]
    fn input_messages() {
        let p = |s: &str| parse_input(s.as_bytes());
        assert_eq!(
            p(r#"{"t":"m","x":100,"y":65535}"#),
            Some(Command::Event(InputEvent::MouseAbs { x: 100, y: 65535 }))
        );
        assert_eq!(
            p(r#"{"t":"r","dx":-5,"dy":7}"#),
            Some(Command::Event(InputEvent::MouseRel { dx: -5, dy: 7 }))
        );
        assert_eq!(
            p(r#"{"t":"b","c":272,"p":true}"#),
            Some(Command::Event(InputEvent::Button {
                code: 272,
                pressed: true
            }))
        );
        assert_eq!(
            p(r#"{"t":"w","dx":0,"dy":-120}"#),
            Some(Command::Event(InputEvent::Scroll { dx: 0, dy: -120 }))
        );
        assert_eq!(
            p(r#"{"t":"k","c":30,"p":false}"#),
            Some(Command::Event(InputEvent::Key {
                code: 30,
                pressed: false
            }))
        );
        assert_eq!(
            p(r#"{"t":"pb","n":1,"c":304,"p":true}"#),
            Some(Command::Event(InputEvent::PadButton {
                pad: 1,
                code: 304,
                pressed: true
            }))
        );
        assert_eq!(
            p(r#"{"t":"pa","n":0,"a":5,"v":255}"#),
            Some(Command::Event(InputEvent::PadAxis {
                pad: 0,
                axis: PadAxis::RightTrigger,
                value: 255
            }))
        );
        for bad in [
            r#"{"t":"pb","n":4,"c":304,"p":true}"#,
            r#"{"t":"pb","n":0,"c":272,"p":true}"#,
            r#"{"t":"pa","n":0,"a":5,"v":256}"#,
            r#"{"t":"pa","n":0,"a":9,"v":0}"#,
        ] {
            assert_eq!(p(bad), None, "{bad}");
        }
        assert_eq!(p(r#"{"t":"release"}"#), Some(Command::ReleaseAll));
        assert_eq!(p(r#"{"t":"keyframe"}"#), Some(Command::Keyframe));
        assert_eq!(p(r#"{"t":"bye"}"#), Some(Command::Bye));
        assert_eq!(p(r#"{"t":"monitor","i":1}"#), Some(Command::Monitor(1)));
        assert_eq!(p(r#"{"t":"monitor","i":8}"#), None);
        assert_eq!(p(r#"{"t":"monitor"}"#), None);
        // Out of range, missing, wrong type, unknown, not JSON.
        for bad in [
            r#"{"t":"m","x":70000,"y":0}"#,
            r#"{"t":"m","x":-1,"y":0}"#,
            r#"{"t":"b","c":30,"p":true}"#,
            r#"{"t":"k","c":0,"p":true}"#,
            r#"{"t":"k","c":1000,"p":true}"#,
            r#"{"t":"k","c":30}"#,
            r#"{"t":"k","c":"30","p":true}"#,
            r#"{"t":"w","dx":0,"dy":99999999}"#,
            r#"{"t":"x"}"#,
            r#"{}"#,
            "rm -rf",
        ] {
            assert_eq!(p(bad), None, "{bad}");
        }
    }
}
