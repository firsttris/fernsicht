//! The client's side of reliable input over UDP.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use fernsicht_proto::{InputEvent, MAX_INPUT_EVENTS, PadAxis};

/// How often unacknowledged events are sent again.
pub const RESEND_AFTER: Duration = Duration::from_millis(15);

/// How long to wait for an acknowledgement before sending again, from the
/// measured round trip: a few round trips (an ack is not sent at once),
/// at least 4 ms, at most [`RESEND_AFTER`]. In a LAN with 0.5 ms round
/// trips a lost key press goes again after 4 ms instead of 15.
pub fn resend_wait(rtt: Option<Duration>) -> Duration {
    rtt.map_or(RESEND_AFTER, |r| {
        (r * 3).clamp(Duration::from_millis(4), RESEND_AFTER)
    })
}

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
    /// How long unacknowledged events wait before they go again.
    resend_after: Duration,
    /// Keys and buttons currently held, by Linux code.
    held: BTreeSet<u16>,
    /// Gamepad buttons held, and axes away from rest, per pad.
    pad_held: BTreeSet<(u8, u16)>,
    pad_axes: BTreeMap<(u8, PadAxis), i32>,
}

impl Default for InputQueue {
    fn default() -> Self {
        Self {
            next_seq: 1,
            entries: VecDeque::new(),
            last_sent: None,
            resend_after: RESEND_AFTER,
            held: BTreeSet::new(),
            pad_held: BTreeSet::new(),
            pad_axes: BTreeMap::new(),
        }
    }
}

impl InputQueue {
    pub fn push(&mut self, event: InputEvent) {
        // The host refuses a whole packet with an invalid event, and the
        // packet is sent again and again: everything after would wait.
        if event.validate().is_err() {
            return;
        }
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
            InputEvent::PadButton { pad, code, pressed } => {
                if pressed == self.pad_held.contains(&(pad, code)) {
                    return;
                }
                if pressed {
                    self.pad_held.insert((pad, code));
                } else {
                    self.pad_held.remove(&(pad, code));
                }
            }
            InputEvent::PadAxis { pad, axis, value } => {
                let rest = 0;
                let old = self.pad_axes.get(&(pad, axis)).copied().unwrap_or(rest);
                if old == value {
                    return;
                }
                if value == rest {
                    self.pad_axes.remove(&(pad, axis));
                } else {
                    self.pad_axes.insert((pad, axis), value);
                }
                // Sticks move at 1 kHz: an unsent value of the same axis
                // is replaced (see `coalesces`).
                if let Some(i) = self.coalesces(pad, axis, value) {
                    self.entries[i].event = event;
                    return;
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

    /// The unsent entry of `pad`'s `axis` that a new `value` may replace:
    /// only analog movement (no d-pad, nothing at rest on either side, so
    /// a tap 0 → 255 → 0 stays a tap), and only if no button of the same
    /// pad comes after it (the order of a move and a press stays).
    fn coalesces(&self, pad: u8, axis: PadAxis, value: i32) -> Option<usize> {
        if matches!(axis, PadAxis::DpadX | PadAxis::DpadY) || value == 0 {
            return None;
        }
        let i = self.entries.iter().rposition(|e| {
            matches!(e.event, InputEvent::PadAxis { pad: p, axis: a, .. } if p == pad && a == axis)
        })?;
        let e = &self.entries[i];
        let moving = matches!(e.event, InputEvent::PadAxis { value: v, .. } if v != 0);
        let reordered = self
            .entries
            .range(i + 1..)
            .any(|later| matches!(later.event, InputEvent::PadButton { pad: p, .. } if p == pad));
        (!e.sent && moving && !reordered).then_some(i)
    }

    /// Releases every held key and button, and centers the gamepads.
    pub fn release_all(&mut self) {
        let pad_held: Vec<(u8, u16)> = self.pad_held.iter().copied().collect();
        for (pad, code) in pad_held {
            self.push(InputEvent::PadButton {
                pad,
                code,
                pressed: false,
            });
        }
        let axes: Vec<(u8, PadAxis)> = self.pad_axes.keys().copied().collect();
        for (pad, axis) in axes {
            self.push(InputEvent::PadAxis {
                pad,
                axis,
                value: 0,
            });
        }
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

    /// Resends after `wait` from now on (see [`resend_wait`]).
    pub fn set_resend_after(&mut self, wait: Duration) {
        self.resend_after = wait;
    }

    /// The events to send now, if any: when something new is queued, or
    /// when unacknowledged events wait longer than the resend time.
    pub fn due(&mut self, now: Instant) -> Option<Vec<(u32, InputEvent)>> {
        if self.entries.is_empty() {
            return None;
        }
        let fresh = self.entries.iter().any(|e| !e.sent);
        let resend = self
            .last_sent
            .is_none_or(|t| now.duration_since(t) >= self.resend_after);
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
    fn events_the_host_would_refuse_are_never_queued() {
        // One refused event fails its whole packet, and the packet is sent
        // again forever: everything typed after it would wait.
        let mut q = InputQueue::default();
        let t0 = Instant::now();
        q.push(key(0x110, true)); // a mouse button sent as a key
        q.push(InputEvent::Button {
            code: 30,
            pressed: true,
        });
        q.push(InputEvent::PadButton {
            pad: 9,
            code: 0x130,
            pressed: true,
        });
        q.push(key(30, true));
        assert_eq!(q.due(t0).unwrap(), vec![(1, key(30, true))]);
    }

    #[test]
    fn resends_follow_the_round_trip() {
        assert_eq!(resend_wait(None), RESEND_AFTER);
        assert_eq!(
            resend_wait(Some(Duration::from_micros(500))),
            Duration::from_millis(4)
        );
        assert_eq!(
            resend_wait(Some(Duration::from_millis(3))),
            Duration::from_millis(9)
        );
        assert_eq!(resend_wait(Some(Duration::from_millis(100))), RESEND_AFTER);
        let mut q = InputQueue::default();
        q.set_resend_after(Duration::from_millis(4));
        let t0 = Instant::now();
        q.push(key(30, true));
        q.due(t0).unwrap();
        assert!(q.due(t0 + Duration::from_millis(3)).is_none());
        assert!(q.due(t0 + Duration::from_millis(4)).is_some());
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

    #[test]
    fn taps_and_the_order_of_a_pad_survive_coalescing() {
        let mut q = InputQueue::default();
        let t0 = Instant::now();
        let axis = |axis, value| InputEvent::PadAxis {
            pad: 0,
            axis,
            value,
        };
        let a = |pressed| InputEvent::PadButton {
            pad: 0,
            code: 0x130,
            pressed,
        };
        // A d-pad tap before A, and a trigger tap: nothing lost or moved.
        q.push(axis(PadAxis::DpadX, -1));
        q.push(a(true));
        q.push(axis(PadAxis::DpadX, 0));
        q.push(axis(PadAxis::RightTrigger, 255));
        q.push(axis(PadAxis::RightTrigger, 0));
        // A stick move, A released, the stick moves on: still in order.
        q.push(axis(PadAxis::LeftX, 1000));
        q.push(a(false));
        q.push(axis(PadAxis::LeftX, 2000));
        let events: Vec<InputEvent> = q.due(t0).unwrap().into_iter().map(|(_, e)| e).collect();
        assert_eq!(
            events,
            [
                axis(PadAxis::DpadX, -1),
                a(true),
                axis(PadAxis::DpadX, 0),
                axis(PadAxis::RightTrigger, 255),
                axis(PadAxis::RightTrigger, 0),
                axis(PadAxis::LeftX, 1000),
                a(false),
                axis(PadAxis::LeftX, 2000),
            ]
        );
    }

    #[test]
    fn gamepads_send_the_newest_axis_value_and_release() {
        let mut q = InputQueue::default();
        let t0 = Instant::now();
        let axis = |value| InputEvent::PadAxis {
            pad: 0,
            axis: PadAxis::LeftX,
            value,
        };
        let button = |pressed| InputEvent::PadButton {
            pad: 1,
            code: 0x130,
            pressed,
        };
        q.push(axis(100));
        q.push(button(true));
        q.push(axis(200));
        q.push(axis(200));
        q.push(button(true));
        assert_eq!(
            q.due(t0).unwrap(),
            vec![(1, axis(200)), (2, button(true))],
            "one value per axis, repeats dropped"
        );
        // Sent ones are not changed afterwards.
        q.push(axis(300));
        q.release_all();
        let events: Vec<InputEvent> = q
            .due(t0 + RESEND_AFTER)
            .unwrap()
            .into_iter()
            .map(|(_, e)| e)
            .collect();
        // The unsent 300 stays: a value at rest is never folded into
        // another (a quick flick still reaches the host).
        assert_eq!(
            &events[2..],
            [axis(300), button(false), axis(0)],
            "{events:?}"
        );
        q.push(axis(0));
        assert_eq!(
            q.due(t0 + RESEND_AFTER * 3).unwrap().len(),
            5,
            "at rest already"
        );
    }
}
