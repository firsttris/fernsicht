//! Keyframe requests: when the client asks, and when the host already
//! answered.
//!
//! A lost frame breaks every frame after it until the next keyframe, so the
//! client asks the moment it sees the gap, not at the next feedback tick.
//! Requests that left the client before the keyframe they caused could
//! arrive are answered by that keyframe: the host skips them instead of
//! sending one keyframe per request (on lossy Wi-Fi, a keyframe storm).

use std::time::{Duration, Instant};

use crate::frame_newer;

/// Room for a keyframe's size: it takes longer to send than other frames.
const SLACK: Duration = Duration::from_millis(10);

/// Round trips beyond this are taken as this: the client reports its own,
/// and no one streams over a longer one.
const MAX_RTT: Duration = Duration::from_secs(1);

/// How long a keyframe is on its way: the round trip, two frames (encode,
/// send) and some slack.
pub fn in_flight_for(rtt: Duration, frame: Duration) -> Duration {
    rtt.min(MAX_RTT) + frame * 2 + SLACK
}

/// The client's side: when to ask.
#[derive(Clone, Copy, Debug, Default)]
pub struct Asks {
    last: Option<Instant>,
}

impl Asks {
    /// Whether to ask now: at once when the need arises, then again each
    /// time `wait` passed without a keyframe (it or the request was lost).
    pub fn due(&mut self, needed: bool, now: Instant, wait: Duration) -> bool {
        if !needed {
            self.last = None;
            return false;
        }
        if self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) < wait)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// The last keyframe the host sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sent {
    pub frame_id: u32,
    pub at: Instant,
}

/// Whether a request is answered by the keyframe `last` already on its way:
/// it is newer than the last frame the client completed (`None`: none yet,
/// or not known) and was sent within `window`. A client that completed a
/// frame after it has lost it and is answered at once; past `window` the
/// request is answered anyway.
pub fn already_coming(
    last: Option<Sent>,
    client_has: Option<u32>,
    now: Instant,
    window: Duration,
) -> bool {
    last.is_some_and(|k| {
        client_has.is_none_or(|has| frame_newer(k.frame_id, has))
            && now.saturating_duration_since(k.at) < window
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn the_first_ask_goes_at_once_and_repeats_only_after_the_wait() {
        let t = Instant::now();
        let mut a = Asks::default();
        assert!(a.due(true, t, 40 * MS));
        assert!(!a.due(true, t + MS, 40 * MS), "no second ask per packet");
        assert!(!a.due(true, t + 39 * MS, 40 * MS));
        assert!(
            a.due(true, t + 40 * MS, 40 * MS),
            "the keyframe did not come"
        );
    }

    #[test]
    fn a_new_need_asks_at_once() {
        let t = Instant::now();
        let mut a = Asks::default();
        assert!(a.due(true, t, 40 * MS));
        assert!(!a.due(false, t + 5 * MS, 40 * MS), "the keyframe came");
        assert!(a.due(true, t + 6 * MS, 40 * MS), "the next loss asks again");
    }

    #[test]
    fn requests_sent_before_the_keyframe_arrived_are_answered_by_it() {
        let t = Instant::now();
        let k = Some(Sent {
            frame_id: 10,
            at: t,
        });
        // The client lost frame 8 and completed 9 before keyframe 10 came.
        assert!(already_coming(k, Some(9), t + 5 * MS, 30 * MS));
        // Nothing completed yet (session start) or not known (browser).
        assert!(already_coming(k, None, t + 5 * MS, 30 * MS));
    }

    #[test]
    fn a_lost_keyframe_is_sent_again_at_once() {
        let t = Instant::now();
        let k = Some(Sent {
            frame_id: 10,
            at: t,
        });
        // Frame 11 completed: keyframe 10 did not make it.
        assert!(!already_coming(k, Some(11), t + 5 * MS, 30 * MS));
        assert!(!already_coming(k, Some(10), t + 5 * MS, 30 * MS));
    }

    #[test]
    fn past_the_window_a_request_is_answered_anyway() {
        let t = Instant::now();
        let k = Some(Sent {
            frame_id: 10,
            at: t,
        });
        assert!(!already_coming(k, Some(9), t + 30 * MS, 30 * MS));
        assert!(!already_coming(None, Some(9), t, 30 * MS));
    }

    #[test]
    fn frame_ids_wrap() {
        let t = Instant::now();
        let k = Some(Sent { frame_id: 1, at: t });
        assert!(already_coming(k, Some(u32::MAX), t, 30 * MS));
    }

    #[test]
    fn the_window_covers_the_round_trip_and_two_frames() {
        let frame = Duration::from_micros(16_667);
        assert_eq!(in_flight_for(2 * MS, frame), 2 * MS + 2 * frame + SLACK);
        assert_eq!(
            in_flight_for(Duration::from_secs(4000), frame),
            MAX_RTT + 2 * frame + SLACK
        );
    }
}
