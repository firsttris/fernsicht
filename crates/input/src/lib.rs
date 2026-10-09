//! Input events and their injection on the host.
//!
//! Phase 2 adds the backends: uinput (virtual mouse, keyboard and gamepads
//! at kernel level, works under every compositor) for gaming and unattended
//! use, libei / the RemoteDesktop portal for desktop mode. Events are sent
//! 2–3× redundantly; `seq` lets the host drop duplicates.

/// Mouse buttons as Linux input event codes (`BTN_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0x110,
    Right = 0x111,
    Middle = 0x112,
    Side = 0x113,
    Extra = 0x114,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    /// Desktop mode: absolute position, normalized to 0.0–1.0.
    MouseAbsolute {
        x: f32,
        y: f32,
    },
    /// Gaming mode (pointer lock): relative motion in pixels.
    MouseRelative {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// High-resolution wheel, 120 units per notch.
    Scroll {
        dx: i32,
        dy: i32,
    },
    /// Linux key code (`KEY_*`), layout-independent.
    Key {
        code: u16,
        pressed: bool,
    },
}

/// An event with its sequence number for duplicate suppression.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SequencedEvent {
    pub seq: u32,
    pub event: InputEvent,
}

pub trait InputSink: Send {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String>;
}

/// Drops events whose sequence number was already seen (redundant sends).
#[derive(Debug, Default)]
pub struct Dedup {
    last: Option<u32>,
}

impl Dedup {
    /// Returns `true` if `seq` is new.
    pub fn accept(&mut self, seq: u32) -> bool {
        match self.last {
            Some(last) if (seq.wrapping_sub(last) as i32) <= 0 => false,
            _ => {
                self.last = Some(seq);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_drops_repeats() {
        let mut d = Dedup::default();
        assert!(d.accept(1));
        assert!(!d.accept(1));
        assert!(d.accept(2));
        assert!(!d.accept(1));

        let mut wrap = Dedup::default();
        assert!(wrap.accept(u32::MAX - 1));
        assert!(wrap.accept(1));
        assert!(!wrap.accept(u32::MAX));
    }
}
