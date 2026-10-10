//! Bitrate adaptation: the host turns the bitrate down on congestion and
//! back up when the link has been clean for a while, never above what the
//! client asked for.
//!
//! Congestion is what FEC cannot hide: frames lost despite it, the host's
//! own sender falling behind, or very heavy loss. Steady moderate loss
//! (Wi-Fi drops packets at random, not only when full) is FEC's job and
//! does not cost sharpness.
//!
//! Down is quick and up is careful (like TCP): too much bitrate costs
//! lost frames right away, too little only some sharpness. Changes are
//! spaced out, because each one may cost the encoder a keyframe.

use std::time::{Duration, Instant};

/// Loss above this (smoothed) counts as congestion: down hard.
pub const HEAVY_LOSS: f32 = 0.20;
/// Loss above this: down a little (FEC gets expensive).
pub const SOME_LOSS: f32 = 0.15;
/// Loss below this (and no lost frames) counts as a clean link.
pub const CLEAN: f32 = 0.02;
/// Clean this long before going up.
pub const CLEAN_FOR: Duration = Duration::from_secs(5);
/// Shortest time between two changes downwards / upwards.
pub const DOWN_EVERY: Duration = Duration::from_secs(1);
pub const UP_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct RateController {
    ceiling: u32,
    floor: u32,
    current: u32,
    /// Smoothed loss ratio.
    loss: f32,
    last_change: Instant,
    clean_since: Option<Instant>,
}

impl RateController {
    /// Starts at `ceiling` kbit/s (what the client asked for). The floor is
    /// a tenth of it, at least 1 Mbit/s.
    pub fn new(ceiling: u32, now: Instant) -> Self {
        let ceiling = ceiling.max(1);
        Self {
            ceiling,
            floor: (ceiling / 10).max(1000).min(ceiling),
            current: ceiling,
            loss: 0.0,
            last_change: now,
            clean_since: Some(now),
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    /// The smoothed loss the controller works with.
    pub fn loss(&self) -> f32 {
        self.loss
    }

    /// One report: the loss ratio the client saw (before FEC), the frames
    /// it lost despite FEC, and whether the host's sender overflowed since
    /// the last report. Returns the new bitrate when it changes.
    pub fn report(
        &mut self,
        loss: f32,
        frames_lost: u32,
        overflow: bool,
        now: Instant,
    ) -> Option<u32> {
        let loss = if loss.is_finite() {
            loss.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Quick to rise, slower to fall: one bad report counts.
        let weight = if loss > self.loss { 0.5 } else { 0.2 };
        self.loss += (loss - self.loss) * weight;
        let since = now.saturating_duration_since(self.last_change);

        let factor = if overflow || frames_lost > 0 || self.loss > HEAVY_LOSS {
            self.clean_since = None;
            (since >= DOWN_EVERY).then_some(0.7)
        } else if self.loss > SOME_LOSS {
            self.clean_since = None;
            (since >= DOWN_EVERY * 2).then_some(0.85)
        } else if self.loss < CLEAN {
            let clean = *self.clean_since.get_or_insert(now);
            (now.saturating_duration_since(clean) >= CLEAN_FOR && since >= UP_EVERY).then_some(1.15)
        } else {
            self.clean_since = None;
            None
        }?;
        let next = ((f64::from(self.current) * factor) as u32).clamp(self.floor, self.ceiling);
        // Not worth a new encoder for less than 5 %.
        if next.abs_diff(self.current) * 20 < self.current {
            return None;
        }
        self.current = next;
        self.last_change = now;
        if factor > 1.0 {
            // Each step up has to earn its own clean stretch.
            self.clean_since = Some(now);
        }
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(100);

    /// Feeds `n` reports of `loss`, 100 ms apart; returns the changes.
    fn feed(c: &mut RateController, t: &mut Instant, n: usize, loss: f32) -> Vec<u32> {
        (0..n)
            .filter_map(|_| {
                *t += MS;
                c.report(loss, 0, false, *t)
            })
            .collect()
    }

    #[test]
    fn a_clean_link_keeps_the_bitrate() {
        let mut t = Instant::now();
        let mut c = RateController::new(20_000, t);
        assert!(
            feed(&mut c, &mut t, 300, 0.0).is_empty(),
            "already at the ceiling"
        );
        assert!(
            feed(&mut c, &mut t, 300, 0.01).is_empty(),
            "1 % is FEC's job"
        );
        assert!(
            feed(&mut c, &mut t, 300, 0.10).is_empty(),
            "steady 10 % (Wi-Fi) too"
        );
        assert_eq!(c.current(), 20_000);
    }

    #[test]
    fn heavy_loss_goes_down_fast_and_recovers_slowly() {
        let mut t = Instant::now();
        let mut c = RateController::new(20_000, t);
        // 10 % loss for 3 s: down about once a second, to the floor at most.
        let down = feed(&mut c, &mut t, 30, 0.30);
        assert!(down.len() >= 2, "{down:?}");
        assert!(down.windows(2).all(|w| w[1] < w[0]));
        assert!(c.current() < 10_000);
        assert!(c.loss() > HEAVY_LOSS);
        let low = c.current();
        // Clean again: nothing for 5 s, then up in steps 5 s apart.
        assert!(feed(&mut c, &mut t, 49, 0.0).is_empty());
        let up = feed(&mut c, &mut t, 300, 0.0);
        assert!(!up.is_empty() && up[0] > low, "{up:?}");
        assert!(up.len() <= 6, "at most one step per 5 s: {up:?}");
        assert_eq!(c.current(), *up.last().unwrap());
        // Eventually back at the ceiling, never above.
        feed(&mut c, &mut t, 3000, 0.0);
        assert_eq!(c.current(), 20_000);
    }

    #[test]
    fn some_loss_goes_down_gently() {
        let mut t = Instant::now();
        let mut c = RateController::new(20_000, t);
        let down = feed(&mut c, &mut t, 25, 0.17);
        assert_eq!(down.first(), Some(&17_000));
    }

    #[test]
    fn the_floor_holds_and_overflow_counts_as_congestion() {
        let mut t = Instant::now();
        let mut c = RateController::new(20_000, t);
        for _ in 0..100 {
            t += Duration::from_secs(1);
            c.report(0.0, 0, true, t);
        }
        assert_eq!(c.current(), 2_000, "a tenth of the ceiling");
        let small = RateController::new(500, t);
        assert_eq!(small.current(), 500);
        let mut small = small;
        t += Duration::from_secs(2);
        assert_eq!(
            small.report(0.5, 0, false, t),
            None,
            "floor = ceiling below 1 Mbit/s"
        );
    }

    #[test]
    fn nonsense_reports_are_harmless() {
        let mut t = Instant::now();
        let mut c = RateController::new(20_000, t);
        for l in [f32::NAN, f32::INFINITY, -1.0] {
            t += Duration::from_secs(1);
            c.report(l, 0, false, t);
        }
        assert!(c.loss() <= 1.0 && c.loss() >= 0.0);
        let mut zero = RateController::new(0, t);
        assert_eq!(zero.current(), 1);
        assert_eq!(zero.report(1.0, 0, true, t + Duration::from_secs(9)), None);
    }

    #[test]
    fn frames_lost_despite_fec_count_as_congestion() {
        let mut t = Instant::now();
        let mut c = RateController::new(20_000, t);
        t += Duration::from_secs(2);
        assert_eq!(c.report(0.03, 2, false, t), Some(14_000));
        // Not again within a second.
        t += MS;
        assert_eq!(c.report(0.03, 2, false, t), None);
    }
}
