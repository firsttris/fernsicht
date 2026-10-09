//! Per-stage latency bookkeeping for the client overlay.
//!
//! A frame passes six points in time. Host points arrive in the packet
//! header, client points are taken locally; all are expressed in the
//! *client* clock after applying the estimated host→client offset.
//!
//! ```text
//! captured ─Capture─▶ capture_ready ─Encode─▶ encoded ─Netz─▶ received
//!          ─Decode─▶ decoded ─Anzeige─▶ presented
//! ```

/// The stages shown in the overlay, in pipeline order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Capture,
    Encode,
    Network,
    Decode,
    Present,
}

impl Stage {
    pub const ALL: [Stage; 5] = [
        Stage::Capture,
        Stage::Encode,
        Stage::Network,
        Stage::Decode,
        Stage::Present,
    ];

    /// German label as used in the client UI.
    pub const fn label(self) -> &'static str {
        match self {
            Stage::Capture => "Capture",
            Stage::Encode => "Encode",
            Stage::Network => "Netz",
            Stage::Decode => "Decode",
            Stage::Present => "Anzeige",
        }
    }
}

/// Points in time for one frame, all in client-clock microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameTimings {
    pub captured: i64,
    pub capture_ready: i64,
    pub encoded: i64,
    pub received: i64,
    pub decoded: i64,
    pub presented: i64,
}

impl FrameTimings {
    /// Duration of `stage` in microseconds, clamped at zero (clock-offset
    /// estimation error can make cross-machine differences slightly negative).
    pub fn stage_us(&self, stage: Stage) -> u32 {
        let (from, to) = match stage {
            Stage::Capture => (self.captured, self.capture_ready),
            Stage::Encode => (self.capture_ready, self.encoded),
            Stage::Network => (self.encoded, self.received),
            Stage::Decode => (self.received, self.decoded),
            Stage::Present => (self.decoded, self.presented),
        };
        clamp_us(to - from)
    }

    /// Glass-to-glass estimate (capture to present), excluding display scan-out.
    pub fn total_us(&self) -> u32 {
        clamp_us(self.presented - self.captured)
    }
}

fn clamp_us(d: i64) -> u32 {
    d.clamp(0, i64::from(u32::MAX)) as u32
}

/// Summary of one series over the current window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub min: u32,
    pub avg: u32,
    pub p95: u32,
    pub max: u32,
    pub samples: usize,
}

/// Fixed-size ring buffer of samples. No allocation after construction.
#[derive(Clone, Debug)]
pub struct Window<const N: usize> {
    samples: [u32; N],
    len: usize,
    next: usize,
}

impl<const N: usize> Default for Window<N> {
    fn default() -> Self {
        Self {
            samples: [0; N],
            len: 0,
            next: 0,
        }
    }
}

impl<const N: usize> Window<N> {
    pub fn push(&mut self, v: u32) {
        self.samples[self.next] = v;
        self.next = (self.next + 1) % N;
        self.len = (self.len + 1).min(N);
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.next = 0;
    }

    pub fn summary(&self) -> Summary {
        if self.len == 0 {
            return Summary::default();
        }
        let mut sorted = self.samples;
        let s = &mut sorted[..self.len];
        s.sort_unstable();
        let sum: u64 = s.iter().map(|&v| u64::from(v)).sum();
        let p95_idx = ((self.len * 95).div_ceil(100)).saturating_sub(1);
        Summary {
            min: s[0],
            avg: (sum / self.len as u64) as u32,
            p95: s[p95_idx],
            max: s[self.len - 1],
            samples: self.len,
        }
    }
}

/// Default window: two seconds at 60 fps.
pub const WINDOW: usize = 120;

/// Rolling per-stage latency statistics.
#[derive(Clone, Debug, Default)]
pub struct LatencyStats {
    stages: [Window<WINDOW>; 5],
    total: Window<WINDOW>,
}

impl LatencyStats {
    pub fn record(&mut self, t: &FrameTimings) {
        for (i, stage) in Stage::ALL.iter().enumerate() {
            self.stages[i].push(t.stage_us(*stage));
        }
        self.total.push(t.total_us());
    }

    pub fn stage(&self, stage: Stage) -> Summary {
        let i = Stage::ALL.iter().position(|s| *s == stage).unwrap();
        self.stages[i].summary()
    }

    pub fn total(&self) -> Summary {
        self.total.summary()
    }

    pub fn clear(&mut self) {
        self.stages.iter_mut().for_each(Window::clear);
        self.total.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timings() -> FrameTimings {
        FrameTimings {
            captured: 1_000,
            capture_ready: 2_000,
            encoded: 6_000,
            received: 9_000,
            decoded: 11_000,
            presented: 15_000,
        }
    }

    #[test]
    fn stage_durations() {
        let t = timings();
        let got: Vec<u32> = Stage::ALL.iter().map(|s| t.stage_us(*s)).collect();
        assert_eq!(got, vec![1_000, 4_000, 3_000, 2_000, 4_000]);
        assert_eq!(t.total_us(), 14_000);
    }

    #[test]
    fn negative_offsets_clamp_to_zero() {
        let mut t = timings();
        t.received = 5_000; // clock offset estimate slightly off
        assert_eq!(t.stage_us(Stage::Network), 0);
    }

    #[test]
    fn window_summary() {
        let mut w = Window::<4>::default();
        assert_eq!(w.summary().samples, 0);
        for v in [10, 20, 30, 40, 50] {
            w.push(v);
        }
        // 10 was evicted.
        let s = w.summary();
        assert_eq!((s.min, s.avg, s.p95, s.max, s.samples), (20, 35, 50, 50, 4));
    }

    #[test]
    fn p95_of_hundred() {
        let mut w = Window::<100>::default();
        for v in 1..=100 {
            w.push(v);
        }
        assert_eq!(w.summary().p95, 95);
    }

    #[test]
    fn stats_record() {
        let mut stats = LatencyStats::default();
        stats.record(&timings());
        assert_eq!(stats.stage(Stage::Encode).avg, 4_000);
        assert_eq!(stats.total().max, 14_000);
    }
}
