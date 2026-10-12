//! How long frames wait in queues on the way (D3): a congestion signal
//! that comes before loss.
//!
//! Each frame gives a sample: when its first packet arrived (client clock)
//! minus when it was encoded (host clock). The clocks' offset is unknown
//! but constant, so the lowest sample of the last [`BASE_WINDOW`] stands
//! for the path without queues; what a frame takes beyond it, it waited in
//! a queue (a router's, an access point's, the host's own sender). Their
//! skew (tens of ppm) moves the base by well under a millisecond over the
//! window.

use std::collections::VecDeque;

/// How far back the base (the lowest delay) reaches, µs.
pub const BASE_WINDOW_US: u64 = 10_000_000;

#[derive(Debug, Default)]
pub struct QueueDelay {
    /// Samples that may still become the window's minimum, oldest first,
    /// rising: (when, delay).
    lowest: VecDeque<(u64, i64)>,
    /// The interval's samples: sum and count.
    sum: i64,
    count: u32,
}

impl QueueDelay {
    /// One frame's delay (first packet's arrival minus its encoding, on two
    /// clocks), µs, measured at `now_us`.
    pub fn push(&mut self, delay_us: i64, now_us: u64) {
        while self.lowest.back().is_some_and(|&(_, d)| d >= delay_us) {
            self.lowest.pop_back();
        }
        self.lowest.push_back((now_us, delay_us));
        while self
            .lowest
            .front()
            .is_some_and(|&(t, _)| t + BASE_WINDOW_US < now_us)
        {
            self.lowest.pop_front();
        }
        self.sum = self.sum.saturating_add(delay_us);
        self.count += 1;
    }

    /// The interval's frames' average time above the base, µs (0 without
    /// frames); starts the next interval.
    pub fn take_interval(&mut self) -> u32 {
        let (sum, count) = (self.sum, self.count);
        self.sum = 0;
        self.count = 0;
        let Some(&(_, base)) = self.lowest.front() else {
            return 0;
        };
        if count == 0 {
            return 0;
        }
        let above = sum / i64::from(count) - base;
        u32::try_from(above.max(0)).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames 16 ms apart: the delays of one interval, then its report.
    fn interval(q: &mut QueueDelay, t: &mut u64, delays: &[i64]) -> u32 {
        for &d in delays {
            *t += 16_000;
            q.push(d, *t);
        }
        q.take_interval()
    }

    #[test]
    fn a_steady_path_has_no_queue_whatever_the_clock_offset() {
        let (mut q, mut t) = (QueueDelay::default(), 0);
        // Clocks 7 s apart, a 1.2 ms path with a little jitter.
        let offset = -7_000_000;
        let jitter = [0, 150, 80, 300, 40, 120];
        for _ in 0..20 {
            let d: Vec<i64> = jitter.iter().map(|j| offset + 1_200 + j).collect();
            assert!(interval(&mut q, &mut t, &d) < 300);
        }
    }

    #[test]
    fn a_growing_queue_shows_as_its_delay() {
        let (mut q, mut t) = (QueueDelay::default(), 0);
        interval(&mut q, &mut t, &[5_000; 30]);
        // A bottleneck's queue fills: each frame waits 2 ms longer.
        let rising: Vec<i64> = (1..=6).map(|i| 5_000 + i * 2_000).collect();
        let first = interval(&mut q, &mut t, &rising);
        assert_eq!(first, 7_000, "average of 2..12 ms above the base");
        // It drains: back to the base.
        assert_eq!(interval(&mut q, &mut t, &[5_000; 6]), 0);
        assert_eq!(q.take_interval(), 0, "no frames, no report");
    }

    #[test]
    fn the_base_forgets_what_lies_beyond_the_window() {
        let (mut q, mut t) = (QueueDelay::default(), 0);
        // A route that was faster once: after the window, the new path is
        // the base.
        interval(&mut q, &mut t, &[1_000; 10]);
        let longer = vec![9_000; (BASE_WINDOW_US / 16_000) as usize + 10];
        interval(&mut q, &mut t, &longer);
        assert_eq!(interval(&mut q, &mut t, &[9_000; 6]), 0);
        // The window keeps only what can still become its minimum.
        assert!(q.lowest.len() <= 2, "{}", q.lowest.len());
    }
}
