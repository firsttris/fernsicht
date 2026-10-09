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
//! queueing asymmetry, so its offset is used.

use fernsicht_proto::ClockPong;

const WINDOW: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockSample {
    pub offset_us: i64,
    pub rtt_us: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ClockSync {
    samples: [ClockSample; WINDOW],
    len: usize,
    next: usize,
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
        };
        self.samples[self.next] = sample;
        self.next = (self.next + 1) % WINDOW;
        self.len = (self.len + 1).min(WINDOW);
        sample
    }

    fn best(&self) -> Option<ClockSample> {
        self.samples[..self.len]
            .iter()
            .copied()
            .min_by_key(|s| s.rtt_us)
    }

    pub fn is_synced(&self) -> bool {
        self.len > 0
    }

    /// Host clock minus client clock, in µs (0 until the first sample).
    pub fn offset_us(&self) -> i64 {
        self.best().map_or(0, |s| s.offset_us)
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

    #[test]
    fn unsynced_is_zero() {
        let c = ClockSync::new();
        assert!(!c.is_synced());
        assert_eq!(c.offset_us(), 0);
    }
}
