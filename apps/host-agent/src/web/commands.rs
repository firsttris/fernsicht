//! What the browser sends over the data channel: input as JSON.

use fernsicht_proto::{BTN_PAD_FIRST, BTN_PAD_LAST, InputEvent, MAX_PADS, PadAxis};
use serde::Deserialize;

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

/// A message from the browser on the data channel, as JSON:
///
/// ```text
/// {"t":"m","x":0..65535,"y":0..65535,"s":n}  pointer at a position (n numbers them)
/// {"t":"r","dx":..,"dy":..}             pointer moved (captured)
/// {"t":"b","c":272,"p":true}            button (Linux BTN_* code)
/// {"t":"w","dx":0,"dy":120}             wheel, 120 per notch
/// {"t":"k","c":30,"p":true}             key (Linux KEY_* code)
/// {"t":"pb","n":0,"c":304,"p":true}     gamepad n's button (BTN_SOUTH..)
/// {"t":"pa","n":0,"a":0,"v":-32768}     gamepad n's axis (ABS_* code)
/// {"t":"monitor","i":1}                 show monitor i
/// {"t":"release"}  {"t":"keyframe"}  {"t":"bye"}
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "t")]
pub(crate) enum FromBrowser {
    #[serde(rename = "m")]
    At { x: u16, y: u16, s: Option<u32> },
    #[serde(rename = "r")]
    Moved { dx: i32, dy: i32 },
    #[serde(rename = "b")]
    Button { c: u16, p: bool },
    #[serde(rename = "w")]
    Wheel { dx: i32, dy: i32 },
    #[serde(rename = "k")]
    Key { c: u16, p: bool },
    #[serde(rename = "pb")]
    PadButton { n: u8, c: u16, p: bool },
    #[serde(rename = "pa")]
    PadAxis { n: u8, a: u16, v: i32 },
    #[serde(rename = "monitor")]
    Monitor { i: u8 },
    #[serde(rename = "release")]
    Release,
    #[serde(rename = "keyframe")]
    Keyframe,
    #[serde(rename = "bye")]
    Bye,
}

/// Parses one message from the browser. Anything out of range is refused,
/// as in the native protocol.
pub(crate) fn parse_input(data: &[u8]) -> Option<Command> {
    command(serde_json::from_slice(data).ok()?)
}

/// What a message asks for, if its values are in range.
pub(crate) fn command(msg: FromBrowser) -> Option<Command> {
    const MOVE: i32 = 1 << 16;
    const WHEEL: i32 = 120 * 100;
    let within = |v: i32, max: i32| (-max..=max).contains(&v).then_some(v);
    let pad = |n: u8| (n < MAX_PADS).then_some(n);
    let event = match msg {
        FromBrowser::At { x, y, s } => {
            let at = InputEvent::MouseAbs { x, y };
            return Some(match s {
                Some(seq) => Command::Position(at, seq),
                None => Command::Event(at),
            });
        }
        FromBrowser::Moved { dx, dy } => InputEvent::MouseRel {
            dx: within(dx, MOVE)?,
            dy: within(dy, MOVE)?,
        },
        FromBrowser::Button { c, p } => InputEvent::Button {
            code: (0x110..=0x117).contains(&c).then_some(c)?,
            pressed: p,
        },
        FromBrowser::Wheel { dx, dy } => InputEvent::Scroll {
            dx: within(dx, WHEEL)?,
            dy: within(dy, WHEEL)?,
        },
        FromBrowser::Key { c, p } => InputEvent::Key {
            code: (1..=0x2ff).contains(&c).then_some(c)?,
            pressed: p,
        },
        FromBrowser::PadButton { n, c, p } => InputEvent::PadButton {
            pad: pad(n)?,
            code: (BTN_PAD_FIRST..=BTN_PAD_LAST).contains(&c).then_some(c)?,
            pressed: p,
        },
        FromBrowser::PadAxis { n, a, v } => {
            let axis = PadAxis::from_code(a)?;
            InputEvent::PadAxis {
                pad: pad(n)?,
                axis,
                value: axis.range().contains(&v).then_some(v)?,
            }
        }
        FromBrowser::Monitor { i } => {
            return (usize::from(i) < fernsicht_proto::MAX_MONITORS).then_some(Command::Monitor(i));
        }
        FromBrowser::Release => return Some(Command::ReleaseAll),
        FromBrowser::Keyframe => return Some(Command::Keyframe),
        FromBrowser::Bye => return Some(Command::Bye),
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
