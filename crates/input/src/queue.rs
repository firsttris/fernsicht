//! The client's side of reliable input over UDP.

use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use fernsicht_proto::{InputEvent, MAX_INPUT_EVENTS};

/// How often unacknowledged events are sent again.
pub const RESEND_AFTER: Duration = Duration::from_millis(15);

#[derive(Clone, Copy, Debug)]
struct Entry {
    seq: u32,
    event: InputEvent,
    /// Sent at least once (then it must not be merged with newer events:
    /// the host may have applied it already).
    sent: bool,
}

/// Events waiting for the host's acknowledgement, oldest first.
///
/// Pointer positions replace each other (only the newest matters);
/// relative motion and wheel steps add up while not yet sent. Pressed keys
/// and buttons are remembered, so all of them can be released at once
/// (window lost focus).
#[derive(Debug)]
pub struct InputQueue {
    next_seq: u32,
    entries: VecDeque<Entry>,
    last_sent: Option<Instant>,
    /// Keys and buttons currently held, by Linux code.
    held: BTreeSet<u16>,
}

impl Default for InputQueue {
    fn default() -> Self {
        Self {
            next_seq: 1,
            entries: VecDeque::new(),
            last_sent: None,
            held: BTreeSet::new(),
        }
    }
}

impl InputQueue {
    pub fn push(&mut self, event: InputEvent) {
        match event {
            InputEvent::Key { code, pressed } | InputEvent::Button { code, pressed } => {
                // A repeat of the current state adds nothing (key repeat is
                // the host's business).
                if pressed == self.held.contains(&code) {
                    return;
                }
                if pressed {
                    self.held.insert(code);
                } else {
                    self.held.remove(&code);
                }
            }
            _ => {}
        }
        if let Some(back) = self.entries.back_mut() {
            match (&mut back.event, event) {
                // Only the newest position matters, sent or not: a newer
                // sequence number supersedes it.
                (InputEvent::MouseAbs { .. }, InputEvent::MouseAbs { .. }) => {
                    self.entries.pop_back();
                }
                (InputEvent::MouseRel { dx, dy }, InputEvent::MouseRel { dx: ndx, dy: ndy })
                | (InputEvent::Scroll { dx, dy }, InputEvent::Scroll { dx: ndx, dy: ndy })
                    if !back.sent =>
                {
                    *dx = dx.saturating_add(ndx);
                    *dy = dy.saturating_add(ndy);
                    return;
                }
                _ => {}
            }
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.entries.push_back(Entry {
            seq,
            event,
            sent: false,
        });
    }

    /// Releases every held key and button.
    pub fn release_all(&mut self) {
        let held: Vec<u16> = self.held.iter().copied().collect();
        for code in held {
            let event = if (fernsicht_proto::BTN_MOUSE_FIRST..=fernsicht_proto::BTN_MOUSE_LAST)
                .contains(&code)
            {
                InputEvent::Button {
                    code,
                    pressed: false,
                }
            } else {
                InputEvent::Key {
                    code,
                    pressed: false,
                }
            };
            self.push(event);
        }
    }

    /// The host applied everything up to `seq`.
    pub fn ack(&mut self, seq: u32) {
        while self
            .entries
            .front()
            .is_some_and(|e| (seq.wrapping_sub(e.seq) as i32) >= 0)
        {
            self.entries.pop_front();
        }
    }

    /// The events to send now, if any: when something new is queued, or
    /// when unacknowledged events wait longer than [`RESEND_AFTER`].
    pub fn due(&mut self, now: Instant) -> Option<Vec<(u32, InputEvent)>> {
        if self.entries.is_empty() {
            return None;
        }
        let fresh = self.entries.iter().any(|e| !e.sent);
        let resend = self
            .last_sent
            .is_none_or(|t| now.duration_since(t) >= RESEND_AFTER);
        if !fresh && !resend {
            return None;
        }
        self.last_sent = Some(now);
        Some(
            self.entries
                .iter_mut()
                .take(MAX_INPUT_EVENTS)
                .map(|e| {
                    e.sent = true;
                    (e.seq, e.event)
                })
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Keys and buttons held right now.
    pub fn held(&self) -> impl Iterator<Item = u16> + '_ {
        self.held.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u16, pressed: bool) -> InputEvent {
        InputEvent::Key { code, pressed }
    }

    #[test]
    fn events_are_sent_until_acknowledged() {
        let mut q = InputQueue::default();
        let t0 = Instant::now();
        q.push(key(30, true));
        q.push(key(30, false));
        let first = q.due(t0).unwrap();
        assert_eq!(first, vec![(1, key(30, true)), (2, key(30, false))]);
        assert_eq!(q.due(t0), None, "nothing new, not yet time to resend");
        // Lost: sent again after a moment, same sequence numbers.
        assert_eq!(q.due(t0 + RESEND_AFTER).unwrap(), first);
        q.ack(1);
        assert_eq!(
            q.due(t0 + RESEND_AFTER * 2).unwrap(),
            vec![(2, key(30, false))]
        );
        q.ack(2);
        assert!(q.is_empty());
        assert_eq!(q.due(t0 + RESEND_AFTER * 3), None);
    }

    #[test]
    fn new_events_go_out_at_once_with_the_unacknowledged() {
        let mut q = InputQueue::default();
        let t0 = Instant::now();
        q.push(key(30, true));
        q.due(t0).unwrap();
        q.push(key(31, true));
        let both = q.due(t0).unwrap();
        assert_eq!(both.len(), 2, "the new one goes right away, with the old");
    }

    #[test]
    fn pointer_positions_collapse_to_the_newest() {
        let mut q = InputQueue::default();
        let t0 = Instant::now();
        q.push(InputEvent::MouseAbs { x: 1, y: 1 });
        q.due(t0).unwrap();
        q.push(InputEvent::MouseAbs { x: 2, y: 2 });
        q.push(InputEvent::MouseAbs { x: 3, y: 3 });
        let out = q.due(t0).unwrap();
        assert_eq!(out, vec![(3, InputEvent::MouseAbs { x: 3, y: 3 })]);
        // A click keeps the position before it.
        q.push(InputEvent::Button {
            code: 0x110,
            pressed: true,
        });
        q.push(InputEvent::MouseAbs { x: 4, y: 4 });
        let out = q.due(t0).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].1, InputEvent::MouseAbs { x: 3, y: 3 });
    }

    #[test]
    fn relative_motion_adds_up_only_until_sent() {
        let mut q = InputQueue::default();
        let t0 = Instant::now();
        q.push(InputEvent::MouseRel { dx: 1, dy: 2 });
        q.push(InputEvent::MouseRel { dx: 3, dy: 4 });
        assert_eq!(
            q.due(t0).unwrap(),
            vec![(1, InputEvent::MouseRel { dx: 4, dy: 6 })]
        );
        // Already sent: the host may have applied it, so no merging.
        q.push(InputEvent::MouseRel { dx: 1, dy: 1 });
        let out = q.due(t0).unwrap();
        assert_eq!(out.len(), 2);
        q.push(InputEvent::Scroll { dx: 0, dy: 120 });
        q.push(InputEvent::Scroll { dx: 0, dy: 120 });
        assert_eq!(
            q.due(t0).unwrap().last().unwrap().1,
            InputEvent::Scroll { dx: 0, dy: 240 }
        );
    }

    #[test]
    fn repeats_are_dropped_and_everything_held_can_be_released() {
        let mut q = InputQueue::default();
        q.push(key(42, true));
        q.push(key(42, true)); // key repeat
        q.push(InputEvent::Button {
            code: 0x110,
            pressed: true,
        });
        q.push(key(30, false)); // never pressed
        assert_eq!(q.held().collect::<Vec<_>>(), vec![42, 0x110]);
        q.release_all();
        assert_eq!(q.held().count(), 0);
        let out: Vec<InputEvent> = q
            .due(Instant::now())
            .unwrap()
            .into_iter()
            .map(|(_, e)| e)
            .collect();
        assert_eq!(
            out,
            vec![
                key(42, true),
                InputEvent::Button {
                    code: 0x110,
                    pressed: true
                },
                key(42, false),
                InputEvent::Button {
                    code: 0x110,
                    pressed: false
                },
            ]
        );
    }

    #[test]
    fn at_most_a_packet_full_and_acks_wrap() {
        let mut q = InputQueue {
            next_seq: u32::MAX - 2,
            ..InputQueue::default()
        };
        for i in 0..100u16 {
            q.push(key(1 + i % 2, i % 4 < 2));
        }
        let out = q.due(Instant::now()).unwrap();
        assert_eq!(out.len(), MAX_INPUT_EVENTS);
        assert_eq!(out[0].0, u32::MAX - 2);
        // Acknowledging across the wrap drops the right ones.
        q.ack(1);
        assert_eq!(q.due(Instant::now() + RESEND_AFTER).unwrap()[0].0, 2);
    }
}
