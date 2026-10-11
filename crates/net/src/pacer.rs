//! Spreads a frame's datagrams over time so that switch, access-point and
//! NIC buffers don't overflow, without holding back what they absorb.
//!
//! A token bucket in its GCRA form: up to `burst_bytes` leave back-to-back
//! (a P-frame usually fits whole, and goes at line rate), the rest at the
//! pacing rate. The bucket refills between frames. A frame that would not
//! leave within its budget at that rate goes faster, so that the sender
//! never falls a frame behind. Before a packet that must wait, the thread
//! sleeps (coarse) and then spins (fine).

use std::time::{Duration, Instant};

/// What the buffers on the way absorb at once, by default: about 45
/// datagrams.
pub const BURST_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Pacer {
    /// Pacing rate in bytes per second (e.g. 1 Gbit/s LAN ≈ 125 MB/s; pace
    /// below the link rate to leave headroom).
    rate_bytes_per_sec: u64,
    /// Bytes that may leave back-to-back.
    burst_bytes: usize,
    /// When everything sent so far would have left at the pacing rate
    /// (GCRA's theoretical arrival time); `None` before the first packet.
    debt_until: Option<Instant>,
}

impl Default for Pacer {
    fn default() -> Self {
        // 400 Mbit/s
        Self::new(50_000_000, BURST_BYTES)
    }
}

impl Pacer {
    pub fn new(rate_bytes_per_sec: u64, burst_bytes: usize) -> Self {
        Self {
            rate_bytes_per_sec,
            burst_bytes,
            debt_until: None,
        }
    }

    /// Sends `packets` through `send`: what the bucket holds at once, the
    /// rest paced so that the frame leaves within `budget`.
    pub fn pace<P, F, E>(&mut self, packets: &[P], budget: Duration, mut send: F) -> Result<(), E>
    where
        P: AsRef<[u8]>,
        F: FnMut(&[u8]) -> Result<(), E>,
    {
        let bytes = packets.iter().map(|p| p.as_ref().len()).sum();
        let rate = rate_for(self.rate_bytes_per_sec, self.burst_bytes, bytes, budget);
        for pkt in packets {
            let pkt = pkt.as_ref();
            let (at, debt) = admit(
                self.debt_until,
                Instant::now(),
                pkt.len(),
                rate,
                self.burst_bytes,
            );
            wait_until(at);
            self.debt_until = Some(debt);
            send(pkt)?;
        }
        Ok(())
    }
}

/// The rate for a frame of `bytes`, bytes per second: `rate`, or faster
/// when what the burst does not cover would not leave within `budget`.
pub fn rate_for(rate: u64, burst: usize, bytes: usize, budget: Duration) -> f64 {
    let paced = bytes.saturating_sub(burst) as f64;
    let needed = paced / budget.as_secs_f64().max(1e-6);
    (rate.max(1) as f64).max(needed)
}

/// GCRA: when a packet of `len` bytes may go, with `burst` bytes allowed
/// ahead of `rate`, and the debt after it. `debt` is when everything sent
/// before would have left at the rate; a debt in the past is a full bucket.
pub fn admit(
    debt: Option<Instant>,
    now: Instant,
    len: usize,
    rate: f64,
    burst: usize,
) -> (Instant, Instant) {
    let ahead = Duration::from_secs_f64(burst as f64 / rate);
    let due = debt.map_or(now, |d| d.max(now));
    let at = due.checked_sub(ahead).map_or(now, |t| t.max(now));
    (at, due + Duration::from_secs_f64(len as f64 / rate))
}

/// Sleeps for most of the wait, then spins for the last stretch (the
/// kernel's sleep granularity is ~50–100 µs at best).
pub fn wait_until(deadline: Instant) {
    const SPIN: Duration = Duration::from_micros(200);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let left = deadline - now;
        if left > SPIN {
            std::thread::sleep(left - SPIN);
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: f64 = 1_000_000.0;

    /// When each of `n` packets of `size` bytes may go (from `t`, no debt),
    /// sent the moment they may.
    fn schedule(t: Instant, n: usize, size: usize, rate: f64, burst: usize) -> Vec<Duration> {
        let (mut debt, mut now) = (None, t);
        (0..n)
            .map(|_| {
                let (at, d) = admit(debt, now, size, rate, burst);
                (debt, now) = (Some(d), at);
                at - t
            })
            .collect()
    }

    #[test]
    fn a_frame_within_the_burst_leaves_at_once() {
        let t = Instant::now();
        // 40 kB (a 1080p P-frame) with a 64 KiB burst at 1 MB/s.
        let s = schedule(t, 29, 1_400, MB, BURST_BYTES);
        assert!(s.iter().all(|d| d.is_zero()), "{s:?}");
    }

    #[test]
    fn beyond_the_burst_packets_follow_the_rate() {
        let t = Instant::now();
        // 1 byte per µs, burst 10 kB, packets of 1 kB.
        let s = schedule(t, 30, 1_000, MB, 10_000);
        // The first ~11 packets at once (the burst plus the one that fills it).
        assert!(s[..11].iter().all(|d| d.is_zero()), "{s:?}");
        // Then one packet per millisecond.
        let step = s[20] - s[19];
        assert!(step.abs_diff(Duration::from_millis(1)) < Duration::from_micros(2));
        assert!(s.is_sorted());
    }

    #[test]
    fn the_bucket_refills_between_frames() {
        let t = Instant::now();
        let (_, debt) = admit(None, t, 50_000, MB, 10_000);
        // 50 kB at 1 MB/s: 50 ms of debt. 10 ms later only part refilled.
        let (at, _) = admit(Some(debt), t + Duration::from_millis(10), 1_000, MB, 10_000);
        assert!(at > t + Duration::from_millis(10));
        // After the debt and the burst's worth, a full burst again.
        let later = t + Duration::from_millis(70);
        let (at, _) = admit(Some(debt), later, 1_000, MB, 10_000);
        assert_eq!(at, later);
    }

    #[test]
    fn a_frame_too_large_for_the_budget_goes_faster() {
        let budget = Duration::from_millis(16);
        // 1 MB/s leaves 16 kB + burst within 16 ms; a 400 kB frame needs more.
        assert_eq!(rate_for(1_000_000, 64_000, 70_000, budget), MB);
        let fast = rate_for(1_000_000, 64_000, 400_000, budget);
        assert!((fast - 336_000.0 / 0.016).abs() < 1.0, "{fast}");
    }

    #[test]
    fn pace_sends_everything_within_budget() {
        let mut p = Pacer::new(10_000_000, 4_000);
        let packets = vec![vec![0u8; 1_000]; 24];
        let start = Instant::now();
        let mut sent = 0;
        p.pace(&packets, Duration::from_millis(16), |_| {
            sent += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        let took = start.elapsed();
        assert_eq!(sent, 24);
        // 20 kB past the burst at 10 MB/s = 2 ms; allow scheduler slack.
        assert!(took >= Duration::from_micros(1_700), "{took:?}");
        assert!(took < Duration::from_millis(50), "{took:?}");
    }

    #[test]
    fn a_small_frame_is_not_held_back() {
        // A slow pacing rate, but the frame fits the burst.
        let mut p = Pacer::new(1_000_000, BURST_BYTES);
        let packets = vec![vec![0u8; 1_400]; 30];
        let start = Instant::now();
        p.pace(&packets, Duration::from_millis(16), |_| Ok::<_, ()>(()))
            .unwrap();
        assert!(start.elapsed() < Duration::from_millis(5));
    }

    #[test]
    fn send_errors_stop_pacing() {
        let packets = vec![vec![0u8; 10]; 5];
        let mut n = 0;
        let r = Pacer::default().pace(&packets, Duration::from_millis(1), |_| {
            n += 1;
            if n == 3 { Err("boom") } else { Ok(()) }
        });
        assert_eq!(r, Err("boom"));
        assert_eq!(n, 3);
    }
}
