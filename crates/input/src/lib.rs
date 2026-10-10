//! Input events from the client, and their injection on the host.
//!
//! - [`InputQueue`] (client): events waiting for the host's acknowledgement;
//!   every packet carries all of them, so a lost packet costs nothing.
//! - [`Dedup`] (host): applies each sequence number once.
//! - [`uinput`] (feature `uinput`, host): virtual keyboard and mice at
//!   kernel level; works under every compositor and on the login screen.
//! - [`gamepad`] (feature `gamepad`, client): the machine's gamepads,
//!   read from evdev.
//! - [`Recorder`]: a sink that only records, for tests.

#[cfg(feature = "gamepad")]
pub mod gamepad;
mod queue;
#[cfg(feature = "uinput")]
pub mod uinput;

use std::sync::{Arc, Mutex};

pub use fernsicht_proto::InputEvent;
pub use queue::InputQueue;

/// Linux button codes for the mouse buttons.
pub mod buttons {
    pub const LEFT: u16 = 0x110;
    pub const RIGHT: u16 = 0x111;
    pub const MIDDLE: u16 = 0x112;
    pub const SIDE: u16 = 0x113;
    pub const EXTRA: u16 = 0x114;
}

/// Where input events end up on the host.
pub trait InputSink: Send {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String>;

    /// Lets go of every key and button still held (session over, client
    /// gone): nothing may stay pressed on the host.
    fn release_all(&mut self);
}

/// Records events instead of injecting them (tests, `--input record`).
#[derive(Clone, Debug, Default)]
pub struct Recorder(pub Arc<Mutex<Vec<InputEvent>>>);

impl InputSink for Recorder {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        self.0.lock().map_err(|e| e.to_string())?.push(*event);
        Ok(())
    }

    fn release_all(&mut self) {}
}

/// Drops events whose sequence number was already applied (they come
/// again until acknowledged).
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

    /// The highest sequence number applied so far.
    pub fn last(&self) -> Option<u32> {
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_drops_repeats() {
        let mut d = Dedup::default();
        assert_eq!(d.last(), None);
        assert!(d.accept(1));
        assert!(!d.accept(1));
        assert!(d.accept(2));
        assert!(!d.accept(1));
        assert_eq!(d.last(), Some(2));

        let mut wrap = Dedup::default();
        assert!(wrap.accept(u32::MAX - 1));
        assert!(wrap.accept(1));
        assert!(!wrap.accept(u32::MAX));
    }

    #[test]
    fn recorder_keeps_events() {
        let mut r = Recorder::default();
        r.inject(&InputEvent::Key {
            code: 30,
            pressed: true,
        })
        .unwrap();
        r.release_all();
        assert_eq!(r.0.lock().unwrap().len(), 1);
    }
}
