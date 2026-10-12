//! The web session's bitrate from the browser's congestion feedback
//! (transport-wide congestion control, TWCC): str0m estimates what the
//! path carries, and the encoder follows it, below the bitrate the session
//! started with. Each change costs the encoder a keyframe, so it moves
//! down at once when the path shrinks a lot, up only after a while and in
//! steps.

use std::time::{Duration, Instant};

/// Share of the estimate the video takes (the rest: sound, headroom).
const SHARE_PERCENT: u64 = 85;
/// Changes smaller than this, in percent, are not worth a keyframe.
const DOWN_BELOW_PERCENT: u64 = 90;
const UP_ABOVE_PERCENT: u64 = 115;
/// Shortest time between changes down and up.
const DOWN_EVERY: Duration = Duration::from_secs(1);
const UP_EVERY: Duration = Duration::from_secs(5);
/// Most a step up multiplies the bitrate by, in percent.
const UP_STEP_PERCENT: u64 = 150;

#[derive(Debug)]
pub(crate) struct WebRate {
    ceiling: u32,
    floor: u32,
    current: u32,
    last_change: Instant,
}

impl WebRate {
    /// Starts at `ceiling` kbit/s (the session's bitrate); never below a
    /// tenth of it (at least 500 kbit/s).
    pub(crate) fn new(ceiling: u32, now: Instant) -> Self {
        let ceiling = ceiling.max(1);
        Self {
            ceiling,
            floor: (ceiling / 10).max(500).min(ceiling),
            current: ceiling,
            last_change: now,
        }
    }

    /// The path carries `estimate_kbps`: the new bitrate, when it changes.
    pub(crate) fn estimate(&mut self, estimate_kbps: u64, now: Instant) -> Option<u32> {
        let target = (estimate_kbps * SHARE_PERCENT / 100)
            .clamp(u64::from(self.floor), u64::from(self.ceiling));
        let current = u64::from(self.current);
        let since = now.saturating_duration_since(self.last_change);
        let next = if target * 100 < current * DOWN_BELOW_PERCENT && since >= DOWN_EVERY {
            target
        } else if target * 100 > current * UP_ABOVE_PERCENT && since >= UP_EVERY {
            target.min(current * UP_STEP_PERCENT / 100)
        } else {
            return None;
        };
        self.current = next as u32;
        self.last_change = now;
        Some(self.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_narrow_path_lowers_the_bitrate_at_once_and_it_climbs_back_slowly() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut r = WebRate::new(20_000, t0);
        // Wi-Fi gets busy: 8 Mbit/s carry.
        assert_eq!(r.estimate(8_000, at(1)), Some(6_800));
        // Wobbles of the estimate cost no keyframe.
        assert_eq!(r.estimate(7_900, at(2)), None);
        assert_eq!(r.estimate(8_500, at(3)), None);
        // The path widens: up only after 5 s, in steps of at most ×1.5.
        assert_eq!(r.estimate(30_000, at(4)), None);
        assert_eq!(r.estimate(30_000, at(6)), Some(10_200));
        assert_eq!(r.estimate(30_000, at(11)), Some(15_300));
        assert_eq!(
            r.estimate(30_000, at(16)),
            Some(20_000),
            "never above the start"
        );
        assert_eq!(r.estimate(30_000, at(30)), None);
        // A collapse: not below the floor.
        assert_eq!(r.estimate(100, at(31)), Some(2_000));
    }
}
