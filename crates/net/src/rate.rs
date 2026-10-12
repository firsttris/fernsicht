//! Bitrate adaptation: the host turns the bitrate down on congestion and
//! back up when the link has been clean for a while, never above what the
//! client asked for.
//!
//! Congestion is what FEC cannot hide: the host's own sender falling
//! behind, very heavy loss, or frames lost despite FEC. Steady moderate
//! loss (Wi-Fi drops packets at random, not only when full) is FEC's job:
//! it neither lowers the bitrate nor keeps it from coming back up.
//!
//! Queues come before loss (D3): frames that wait on the way (the
//! client's queue delay, [`crate::delay`]) and packets a router marks CE
//! instead of dropping them (ECN) count as congestion too, and a queue
//! that is building keeps the bitrate from going up into it. The
//! thresholds stand clear of Wi-Fi's own jitter of a few milliseconds.
//!
//! Down is quick and up is careful (like TCP): too much bitrate costs
//! lost frames right away, too little only some sharpness. Changes are
//! spaced out, because each one may cost the encoder a keyframe.

use std::time::{Duration, Instant};

#[cfg(doc)]
use crate::LossEstimator;

/// Loss above this (smoothed) counts as congestion: down hard.
pub const HEAVY_LOSS: f32 = 0.20;
/// Loss above this: down a little (FEC gets expensive). Below it, with no
/// frame lost and no overflow, the link counts as clean.
pub const SOME_LOSS: f32 = 0.15;
/// Steps down: hard on congestion, gently for a frame lost now and then.
const DOWN_HARD: f64 = 0.7;
const DOWN_GENTLY: f64 = 0.85;
const UP: f64 = 1.15;
/// Queue delay (µs) from which the bitrate goes down hard, goes down
/// gently, and below which the link counts as clean; in between it holds.
pub const DELAY_HEAVY_US: u32 = 50_000;
pub const DELAY_SOME_US: u32 = 15_000;
pub const DELAY_CLEAN_US: u32 = 5_000;
/// Clean this long before going up.
pub const CLEAN_FOR: Duration = Duration::from_secs(5);
/// Shortest time between two changes downwards / upwards.
pub const DOWN_EVERY: Duration = Duration::from_secs(1);
pub const UP_EVERY: Duration = Duration::from_secs(5);

/// What one client report says about the path.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Signals {
    /// Loss before FEC, smoothed as FEC sizes for it ([`LossEstimator`]).
    pub loss: f32,
    /// Frames lost despite FEC.
    pub frames_lost: u32,
    /// The host's sender overflowed since the last report.
    pub overflow: bool,
    /// How long frames waited in queues on the way, µs.
    pub queue_delay_us: u32,
    /// Packets marked CE (ECN).
    pub ce_packets: u32,
}

#[derive(Debug, Clone)]
pub struct RateController {
    ceiling: u32,
    floor: u32,
    current: u32,
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
            last_change: now,
            clean_since: Some(now),
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    /// One report: the loss the client saw (before FEC), smoothed as FEC
    /// sizes for it ([`LossEstimator`]), the frames it lost despite FEC, and
    /// whether the host's sender overflowed since the last report. Returns
    /// the new bitrate when it changes.
    pub fn report(
        &mut self,
        loss: f32,
        frames_lost: u32,
        overflow: bool,
        now: Instant,
    ) -> Option<u32> {
        self.report_signals(
            &Signals {
                loss,
                frames_lost,
                overflow,
                ..Signals::default()
            },
            now,
        )
    }

    /// [`Self::report`] with every signal, queues included.
    pub fn report_signals(&mut self, s: &Signals, now: Instant) -> Option<u32> {
        let since = now.saturating_duration_since(self.last_change);

        let factor = if s.overflow || s.loss > HEAVY_LOSS || s.queue_delay_us >= DELAY_HEAVY_US {
            self.clean_since = None;
            (since >= DOWN_EVERY).then_some(DOWN_HARD)
        } else if s.frames_lost > 0
            || s.loss > SOME_LOSS
            || s.ce_packets > 0
            || s.queue_delay_us >= DELAY_SOME_US
        {
            self.clean_since = None;
            (since >= DOWN_EVERY * 2).then_some(DOWN_GENTLY)
        } else if s.queue_delay_us >= DELAY_CLEAN_US {
            // A queue is building: no step up into it, none down yet.
            self.clean_since = None;
            None
        } else {
            let clean = *self.clean_since.get_or_insert(now);
            (now.saturating_duration_since(clean) >= CLEAN_FOR && since >= UP_EVERY).then_some(UP)
        }?;
        let next = ((f64::from(self.current) * factor) as u32).clamp(self.floor, self.ceiling);
        // Not worth a new encoder for less than 5 %, except the last step to
        // the ceiling or the floor.
        let at_limit = next == self.ceiling || next == self.floor;
        if next == self.current || (!at_limit && next.abs_diff(self.current) * 20 < self.current) {
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
    use crate::LossEstimator;

    const MS: Duration = Duration::from_millis(100);

    /// The controller and the loss filter its reports go through, as in
    /// the host's session.
    type Link = (RateController, LossEstimator);

    fn link(ceiling: u32, t: Instant) -> Link {
        (RateController::new(ceiling, t), LossEstimator::default())
    }

    fn report(c: &mut Link, loss: f32, lost: u32, overflow: bool, t: Instant) -> Option<u32> {
        let smoothed = c.1.update(loss);
        c.0.report(smoothed, lost, overflow, t)
    }

    /// Feeds `n` reports of `loss`, 100 ms apart; returns the changes.
    fn feed(c: &mut Link, t: &mut Instant, n: usize, loss: f32) -> Vec<u32> {
        (0..n)
            .filter_map(|_| {
                *t += MS;
                report(c, loss, 0, false, *t)
            })
            .collect()
    }

    #[test]
    fn a_clean_link_keeps_the_bitrate() {
        let mut t = Instant::now();
        let mut c = link(20_000, t);
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
        assert_eq!(c.0.current(), 20_000);
    }

    #[test]
    fn heavy_loss_goes_down_fast_and_recovers_slowly() {
        let mut t = Instant::now();
        let mut c = link(20_000, t);
        // 30 % loss for 3 s: down about once a second, to the floor at most.
        let down = feed(&mut c, &mut t, 30, 0.30);
        assert!(down.len() >= 2, "{down:?}");
        assert!(down.windows(2).all(|w| w[1] < w[0]));
        assert!(c.0.current() < 10_000);
        assert!(c.1.estimate() > HEAVY_LOSS);
        let low = c.0.current();
        // Clean again: nothing for 5 s, then up in steps 5 s apart.
        assert!(feed(&mut c, &mut t, 49, 0.0).is_empty());
        let up = feed(&mut c, &mut t, 300, 0.0);
        assert!(!up.is_empty() && up[0] > low, "{up:?}");
        assert!(up.len() <= 6, "at most one step per 5 s: {up:?}");
        assert_eq!(c.0.current(), *up.last().unwrap());
        // Eventually back at the ceiling, never above.
        feed(&mut c, &mut t, 3000, 0.0);
        assert_eq!(c.0.current(), 20_000);
    }

    #[test]
    fn some_loss_goes_down_gently() {
        let mut t = Instant::now();
        let mut c = link(20_000, t);
        let down = feed(&mut c, &mut t, 25, 0.17);
        assert_eq!(down.first(), Some(&17_000));
    }

    #[test]
    fn the_floor_holds_and_overflow_counts_as_congestion() {
        let mut t = Instant::now();
        let mut c = link(20_000, t);
        for _ in 0..100 {
            t += Duration::from_secs(1);
            report(&mut c, 0.0, 0, true, t);
        }
        assert_eq!(c.0.current(), 2_000, "a tenth of the ceiling");
        let small = link(500, t);
        assert_eq!(small.0.current(), 500);
        let mut small = small;
        t += Duration::from_secs(2);
        assert_eq!(
            report(&mut small, 0.5, 0, false, t),
            None,
            "floor = ceiling below 1 Mbit/s"
        );
    }

    #[test]
    fn nonsense_reports_are_harmless() {
        let mut t = Instant::now();
        let mut c = link(20_000, t);
        for l in [f32::NAN, f32::INFINITY, -1.0] {
            t += Duration::from_secs(1);
            report(&mut c, l, 0, false, t);
        }
        assert!(c.1.estimate() <= 1.0 && c.1.estimate() >= 0.0);
        let mut zero = link(0, t);
        assert_eq!(zero.0.current(), 1);
        assert_eq!(
            report(&mut zero, 1.0, 0, true, t + Duration::from_secs(9)),
            None
        );
    }

    #[test]
    fn frames_lost_despite_fec_go_down_gently() {
        let mut t = Instant::now();
        let mut c = link(20_000, t);
        t += Duration::from_secs(2);
        assert_eq!(report(&mut c, 0.03, 2, false, t), Some(17_000));
        // Not again within two seconds.
        t += MS;
        assert_eq!(report(&mut c, 0.03, 2, false, t), None);
    }

    #[test]
    fn wifi_loss_that_fec_hides_does_not_hold_the_bitrate_down() {
        // A frame lost now and then, otherwise steady random loss that FEC
        // repairs: the bitrate comes back to the ceiling.
        for loss in [0.03, 0.10] {
            let mut t = Instant::now();
            let mut c = link(20_000, t);
            t += Duration::from_secs(2);
            assert!(report(&mut c, loss, 1, false, t).is_some());
            feed(&mut c, &mut t, 600, loss);
            assert_eq!(c.0.current(), 20_000, "{loss}");
        }
    }

    /// Reports 100 ms apart with a queue delay and CE marks, no loss.
    fn queued(c: &mut Link, t: &mut Instant, n: usize, delay_us: u32, ce: u32) -> Vec<u32> {
        (0..n)
            .filter_map(|_| {
                *t += MS;
                let loss = c.1.update(0.0);
                c.0.report_signals(
                    &Signals {
                        loss,
                        queue_delay_us: delay_us,
                        ce_packets: ce,
                        ..Signals::default()
                    },
                    *t,
                )
            })
            .collect()
    }

    #[test]
    fn a_queue_lowers_the_bitrate_before_anything_is_lost() {
        let mut t = Instant::now();
        let mut c = link(20_000, t);
        // 20 ms of queue: gently, every 2 s.
        let gentle = queued(&mut c, &mut t, 45, 20_000, 0);
        assert_eq!(gentle.first(), Some(&17_000));
        assert!(gentle.len() <= 3, "{gentle:?}");
        // Bufferbloat (a queue of 80 ms): hard, every second.
        let mut c = link(20_000, t);
        let hard = queued(&mut c, &mut t, 25, 80_000, 0);
        assert_eq!(hard.first(), Some(&14_000));
        assert!(hard.len() >= 2, "{hard:?}");
    }

    #[test]
    fn a_building_queue_holds_the_bitrate_where_it_is() {
        let mut t = Instant::now();
        let mut c = link(20_000, t);
        queued(&mut c, &mut t, 30, 80_000, 0);
        let low = c.0.current();
        // 8 ms of queue for a minute: neither down nor up.
        assert!(queued(&mut c, &mut t, 600, 8_000, 0).is_empty());
        assert_eq!(c.0.current(), low);
        // Wi-Fi's jitter of a few ms is clean: back up.
        assert!(!queued(&mut c, &mut t, 600, 3_000, 0).is_empty());
        assert!(c.0.current() > low);
    }

    #[test]
    fn ce_marks_count_as_congestion_without_loss() {
        let mut t = Instant::now();
        let mut c = link(20_000, t);
        let down = queued(&mut c, &mut t, 25, 0, 3);
        assert_eq!(down.first(), Some(&17_000));
        // And keep it from going up.
        let low = c.0.current();
        assert!(queued(&mut c, &mut t, 600, 0, 1).iter().all(|&k| k <= low));
    }

    #[test]
    fn one_bad_report_does_not_count_as_congestion() {
        let mut t = Instant::now();
        let mut c = link(20_000, t);
        feed(&mut c, &mut t, 30, 0.0);
        t += MS;
        assert_eq!(report(&mut c, 0.40, 0, false, t), None);
        assert!(c.1.estimate() < SOME_LOSS);
        assert!(feed(&mut c, &mut t, 100, 0.0).is_empty());
    }
}
