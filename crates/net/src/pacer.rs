//! Spreads a frame's datagrams over time so switch and NIC buffers don't
//! overflow, while keeping the whole frame inside a fixed budget.
//!
//! Packet `i` is due at `min(bytes_before_i / rate, i * budget / n)`:
//! paced at link rate, but never later than an even spread over the budget.
//! Packets go out in small bursts; between bursts the thread sleeps
//! (coarse) and then spins (fine).

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Pacer {
    /// Pacing rate in bytes per second (e.g. 1 Gbit/s LAN ≈ 125 MB/s; pace
    /// below the link rate to leave headroom).
    pub rate_bytes_per_sec: u64,
    /// Packets sent back-to-back before checking the schedule.
    pub burst: usize,
}

impl Default for Pacer {
    fn default() -> Self {
        Self {
            // 400 Mbit/s
            rate_bytes_per_sec: 50_000_000,
            burst: 4,
        }
    }
}

impl Pacer {
    /// Offset (µs after the first packet) at which packet `index` is due.
    pub fn offset_us(
        &self,
        index: usize,
        bytes_before: usize,
        count: usize,
        budget_us: u64,
    ) -> u64 {
        if count == 0 {
            return 0;
        }
        let by_rate =
            (bytes_before as u64).saturating_mul(1_000_000) / self.rate_bytes_per_sec.max(1);
        let by_budget = index as u64 * budget_us / count as u64;
        by_rate.min(by_budget)
    }

    /// Sends `packets` through `send`, pacing them over at most `budget`.
    pub fn pace<P, F, E>(&self, packets: &[P], budget: Duration, mut send: F) -> Result<(), E>
    where
        P: AsRef<[u8]>,
        F: FnMut(&[u8]) -> Result<(), E>,
    {
        let start = Instant::now();
        let budget_us = budget.as_micros() as u64;
        let burst = self.burst.max(1);
        let mut bytes_before = 0usize;
        for (i, pkt) in packets.iter().enumerate() {
            if i % burst == 0 && i > 0 {
                let due = start
                    + Duration::from_micros(self.offset_us(
                        i,
                        bytes_before,
                        packets.len(),
                        budget_us,
                    ));
                wait_until(due);
            }
            let pkt = pkt.as_ref();
            send(pkt)?;
            bytes_before += pkt.len();
        }
        Ok(())
    }
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

    #[test]
    fn offsets_follow_rate_until_budget_binds() {
        let p = Pacer {
            rate_bytes_per_sec: 1_000_000, // 1 byte per µs
            burst: 1,
        };
        // 10 packets of 100 bytes, generous budget → paced by rate
        assert_eq!(p.offset_us(5, 500, 10, 100_000), 500);
        // tight budget (1 ms for 10 kB at 1 byte/µs) → evenly spread over budget
        assert_eq!(p.offset_us(5, 5_000, 10, 1_000), 500);
        assert_eq!(p.offset_us(0, 0, 10, 1_000), 0);
    }

    #[test]
    fn pace_sends_everything_within_budget() {
        let p = Pacer {
            rate_bytes_per_sec: 10_000_000,
            burst: 2,
        };
        let packets = vec![vec![0u8; 1_000]; 20];
        let start = Instant::now();
        let mut sent = 0;
        p.pace(&packets, Duration::from_millis(4), |_| {
            sent += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        let took = start.elapsed();
        assert_eq!(sent, 20);
        // 20 kB at 10 MB/s = 2 ms of pacing; allow scheduler slack.
        assert!(took >= Duration::from_micros(1_700), "{took:?}");
        assert!(took < Duration::from_millis(50), "{took:?}");
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
