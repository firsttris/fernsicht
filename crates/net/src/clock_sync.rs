//! NTP-style clock offset estimation between host and client.
//!
//! The client sends `ClockPing{t0}`, the host answers with its receive time
//! `t1` and send time `t2`, the client notes arrival `t3`:
//!
//! ```text
//! offset = ((t1 - t0) + (t2 - t3)) / 2     // host clock − client clock
//! rtt    = (t3 - t0) - (t2 - t1)
//! ```
//!
//! The sample with the smallest RTT in a sliding window has the least
//! queueing asymmetry, so its offset is used. Two crystals drift apart by
//! tens of ppm, a millisecond in a minute: the best samples of the older
//! and the newer half of the window give the skew, and the offset is
//! carried forward to the moment asked for.

use fernsicht_proto::ClockPong;

/// Samples kept: at a ping every 500 ms, half a minute.
const WINDOW: usize = 64;

/// The halves' best samples must lie this far apart for a skew.
const MIN_SPAN_US: u64 = 5_000_000;

/// Skew beyond this is no crystal: ignored.
const MAX_SKEW: f64 = 200e-6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockSample {
    pub offset_us: i64,
    pub rtt_us: u64,
    /// When it arrived, client clock.
    pub at_us: u64,
}

#[derive(Clone, Debug)]
pub struct ClockSync {
    samples: [ClockSample; WINDOW],
    len: usize,
    next: usize,
}

impl Default for ClockSync {
    fn default() -> Self {
        Self {
            samples: [ClockSample::default(); WINDOW],
            len: 0,
            next: 0,
        }
    }
}

impl ClockSync {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a pong received at `client_recv_us` (client clock).
    pub fn on_pong(&mut self, pong: &ClockPong, client_recv_us: u64) -> ClockSample {
        let t0 = pong.client_send_us as i64;
        let t1 = pong.host_recv_us as i64;
        let t2 = pong.host_send_us as i64;
        let t3 = client_recv_us as i64;
        let sample = ClockSample {
            offset_us: ((t1 - t0) + (t2 - t3)) / 2,
            rtt_us: ((t3 - t0) - (t2 - t1)).max(0) as u64,
            at_us: client_recv_us,
        };
        self.samples[self.next] = sample;
        self.next = (self.next + 1) % WINDOW;
        self.len = (self.len + 1).min(WINDOW);
        sample
    }

    /// The samples, oldest first.
    fn in_order(&self) -> impl Iterator<Item = ClockSample> + '_ {
        let start = if self.len == WINDOW { self.next } else { 0 };
        (0..self.len).map(move |i| self.samples[(start + i) % WINDOW])
    }

    fn best(&self) -> Option<ClockSample> {
        self.in_order().min_by_key(|s| s.rtt_us)
    }

    /// The best sample of the newer half and the skew it moves by (host
    /// clock runs faster than ours by this fraction), once the halves' best
    /// samples lie far enough apart.
    fn trend(&self) -> Option<(ClockSample, f64)> {
        let half = self.len / 2;
        let old = self.in_order().take(half).min_by_key(|s| s.rtt_us)?;
        let new = self.in_order().skip(half).min_by_key(|s| s.rtt_us)?;
        let span = new.at_us.checked_sub(old.at_us)?;
        if span < MIN_SPAN_US {
            return None;
        }
        let skew = (new.offset_us - old.offset_us) as f64 / span as f64;
        (skew.abs() <= MAX_SKEW).then_some((new, skew))
    }

    pub fn is_synced(&self) -> bool {
        self.len > 0
    }

    /// Host clock minus client clock at client time `at_us`, in µs: the
    /// best recent sample, carried forward by the skew (0 until the first
    /// sample).
    pub fn offset_at(&self, at_us: u64) -> i64 {
        match self.trend() {
            Some((base, skew)) => {
                let since = at_us as f64 - base.at_us as f64;
                base.offset_us + (since * skew).round() as i64
            }
            None => self.best().map_or(0, |s| s.offset_us),
        }
    }

    /// [`Self::offset_at`] the newest sample's time.
    pub fn offset_us(&self) -> i64 {
        let newest = self.in_order().last().map_or(0, |s| s.at_us);
        self.offset_at(newest)
    }

    /// The measured skew in ppm (host faster than ours: positive), if any.
    pub fn skew_ppm(&self) -> Option<f64> {
        self.trend().map(|(_, skew)| skew * 1e6)
    }

    /// Smallest RTT in the window.
    pub fn rtt_us(&self) -> Option<u64> {
        self.best().map(|s| s.rtt_us)
    }

    /// Converts a host timestamp into the client clock.
    pub fn host_to_client(&self, host_us: u64) -> i64 {
        host_us as i64 - self.offset_us()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pong(t0: u64, t1: u64, t2: u64) -> ClockPong {
        ClockPong {
            seq: 0,
            client_send_us: t0,
            host_recv_us: t1,
            host_send_us: t2,
        }
    }

    #[test]
    fn symmetric_path_gives_exact_offset() {
        // host is 1_000_000 µs ahead, one-way delay 500 µs, host turnaround 20 µs
        let mut c = ClockSync::new();
        let s = c.on_pong(&pong(100, 1_000_600, 1_000_620), 1_120);
        assert_eq!(s.offset_us, 1_000_000);
        assert_eq!(s.rtt_us, 1_000);
        assert_eq!(c.host_to_client(1_005_000), 5_000);
    }

    #[test]
    fn min_rtt_sample_wins() {
        let mut c = ClockSync::new();
        // queued sample: asymmetric delay skews offset
        c.on_pong(&pong(0, 1_005_000, 1_005_000), 5_500);
        // clean sample
        c.on_pong(&pong(10_000, 1_010_250, 1_010_250), 10_500);
        assert_eq!(c.rtt_us(), Some(500));
        assert_eq!(c.offset_us(), 1_000_000);
    }

    /// Pings every 500 ms for `secs` against a host clock running `ppm`
    /// faster, 1 s ahead, with queueing delays of 0–1 ms each way.
    fn drifting(ppm: f64, secs: u64) -> (ClockSync, impl Fn(u64) -> i64) {
        let host = move |client_us: u64| {
            1_000_000 + client_us as i64 + (client_us as f64 * ppm * 1e-6) as i64
        };
        let mut c = ClockSync::new();
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut delay = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            300 + rng % 1_000
        };
        for i in 1..=secs * 2 {
            let t0 = i * 500_000;
            let t1 = t0 + delay();
            let t3 = t1 + 20 + delay();
            let pong = pong(t0, host(t1) as u64, host(t1 + 20) as u64);
            c.on_pong(&pong, t3);
        }
        (c, move |at| host(at) - at as i64)
    }

    #[test]
    fn the_offset_follows_a_drifting_clock() {
        let (c, truth) = drifting(50.0, 60);
        let now = 60_500_000;
        let err = (c.offset_at(now) - truth(now)).abs();
        assert!(err < 100, "{err} µs off with skew");
        let ppm = c.skew_ppm().unwrap();
        assert!((ppm - 50.0).abs() < 10.0, "{ppm} ppm");
        // Without the skew, the best sample is up to half a minute old.
        let stale = (c.best().unwrap().offset_us - truth(now)).abs();
        assert!(stale > 3 * err, "{stale} µs stale vs {err}");
    }

    #[test]
    fn a_short_history_has_no_skew() {
        let (c, _) = drifting(50.0, 3);
        assert_eq!(c.skew_ppm(), None);
        assert_eq!(c.offset_at(10_000_000), c.best().unwrap().offset_us);
    }

    #[test]
    fn unsynced_is_zero() {
        let c = ClockSync::new();
        assert!(!c.is_synced());
        assert_eq!(c.offset_us(), 0);
    }
}
