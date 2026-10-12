//! Spreads a frame's datagrams over time so that switch, access-point and
//! NIC buffers don't overflow, without holding back what they absorb.
//!
//! A token bucket in its GCRA form: up to `burst_bytes` leave back-to-back
//! (a P-frame usually fits whole, and goes at line rate), the rest at the
//! pacing rate. The bucket refills between frames. A frame that would not
//! leave within its budget at that rate goes faster, so that the sender
//! never falls a frame behind.
//!
//! Packets leave in chunks: those due within [`QUANTUM`] of each other, as
//! many as one UDP GSO call carries. One system call and one wake-up per
//! chunk, not per packet; between chunks the thread sleeps, and spins only
//! for the last stretch.

use std::time::{Duration, Instant};

use crate::socket::{MAX_SEGMENTS, MAX_SEGMENTS_BYTES};

/// Packets due this soon after a chunk's first go with it (at most this
/// early): at 400 Mbit/s, 10 kB.
pub const QUANTUM: Duration = Duration::from_micros(200);

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

    /// Paces at `rate_bytes_per_sec` from the next frame on.
    pub fn set_rate(&mut self, rate_bytes_per_sec: u64) {
        self.rate_bytes_per_sec = rate_bytes_per_sec.max(1);
    }

    pub fn rate(&self) -> u64 {
        self.rate_bytes_per_sec
    }

    /// Sends `packets` through `send` in chunks: what the bucket holds at
    /// once, the rest paced so that the frame leaves within `budget`.
    pub fn pace<P, F, E>(&mut self, packets: &[P], budget: Duration, mut send: F) -> Result<(), E>
    where
        P: AsRef<[u8]>,
        F: FnMut(&[P]) -> Result<(), E>,
    {
        let bytes = packets.iter().map(|p| p.as_ref().len()).sum();
        let rate = rate_for(self.rate_bytes_per_sec, self.burst_bytes, bytes, budget);
        let mut rest = packets;
        while !rest.is_empty() {
            let lens = rest.iter().map(|p| p.as_ref().len());
            let (n, at, debt) = next_chunk(
                self.debt_until,
                Instant::now(),
                lens,
                rate,
                self.burst_bytes,
            );
            wait_until(at);
            self.debt_until = Some(debt);
            let (chunk, tail) = rest.split_at(n);
            send(chunk)?;
            rest = tail;
        }
        Ok(())
    }
}

/// The next chunk of packets (by length): how many, when it may go, and
/// the debt after it. It holds those due within [`QUANTUM`] of the first,
/// at most one GSO call's worth.
pub fn next_chunk(
    debt: Option<Instant>,
    now: Instant,
    lens: impl IntoIterator<Item = usize>,
    rate: f64,
    burst: usize,
) -> (usize, Instant, Instant) {
    let mut lens = lens.into_iter();
    let first = lens.next().unwrap_or(0);
    let (at, mut debt) = admit(debt, now, first, rate, burst);
    let (mut n, mut bytes) = (1, first);
    for len in lens {
        if n == MAX_SEGMENTS || bytes + len > MAX_SEGMENTS_BYTES {
            break;
        }
        let (next_at, next_debt) = admit(Some(debt), at, len, rate, burst);
        if next_at > at + QUANTUM {
            break;
        }
        (n, bytes, debt) = (n + 1, bytes + len, next_debt);
    }
    (n, at, debt)
}

/// What the pacing rate may fall to, as a multiple of the bitrate: a
/// frame of twice the average beyond the burst still leaves within half
/// its interval, whatever the measurement got wrong.
pub const MIN_PACE_PER_BITRATE: u64 = 4;

/// The rate to pace at (C3), bytes per second: what the client measured
/// the path deliver (`delivery_kbps`, 0 while nothing was measured: then
/// `cap`), at most `cap` (the configured rate), at least
/// [`MIN_PACE_PER_BITRATE`] times the bitrate. Faster than the slowest link
/// would only queue the rest of the frame there.
pub fn rate_for_delivery(cap: u64, delivery_kbps: u32, bitrate_kbps: u32) -> u64 {
    if delivery_kbps == 0 {
        return cap;
    }
    let to_bytes = |kbps: u64| kbps * 1000 / 8;
    let floor = to_bytes(u64::from(bitrate_kbps) * MIN_PACE_PER_BITRATE);
    to_bytes(u64::from(delivery_kbps)).clamp(floor.min(cap), cap)
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

/// Sleeps for most of the wait, then spins for the last stretch. Hot
/// threads sleep with a timer slack of a microsecond
/// (`fernsicht_core::thread`), so the stretch can be short.
pub fn wait_until(deadline: Instant) {
    const SPIN: Duration = Duration::from_micros(20);
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
    fn paces_at_what_the_path_delivers_within_bounds() {
        let cap = 50_000_000; // 400 Mbit/s
        assert_eq!(rate_for_delivery(cap, 0, 20_000), cap, "nothing measured");
        // Wi-Fi delivering 150 Mbit/s.
        assert_eq!(rate_for_delivery(cap, 150_000, 20_000), 18_750_000);
        assert_eq!(
            rate_for_delivery(cap, 2_000_000, 20_000),
            cap,
            "never above the cap"
        );
        // A measurement far too low: four times the bitrate at least.
        assert_eq!(rate_for_delivery(cap, 30_000, 20_000), 10_000_000);
        // A bitrate the cap cannot carry four times over: the cap.
        assert_eq!(rate_for_delivery(cap, 30_000, 150_000), cap);
        let mut p = Pacer::default();
        p.set_rate(0);
        assert_eq!(p.rate(), 1, "never zero");
    }

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
        let (mut sent, mut chunks) = (0, 0);
        p.pace(&packets, Duration::from_millis(16), |chunk| {
            sent += chunk.len();
            chunks += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        let took = start.elapsed();
        assert_eq!(sent, 24);
        // The burst and what is due within the quantum at once (7), then
        // a chunk per 200 µs (3 packets each): 7 chunks, fewer where the
        // thread is late and finds more due at once.
        assert!((2..=7).contains(&chunks), "{chunks} chunks");
        // 20 kB past the burst at 10 MB/s = 2 ms, the last chunk up to a
        // quantum early; allow scheduler slack.
        assert!(took >= Duration::from_micros(1_500), "{took:?}");
        assert!(took < Duration::from_millis(50), "{took:?}");
    }

    #[test]
    fn the_burst_goes_in_one_call_and_the_rest_by_the_quantum() {
        let t = Instant::now();
        let mtu = 1_400;
        // 30 packets fit the 64 KiB burst: one chunk.
        assert_eq!(next_chunk(None, t, [mtu; 30], 50e6, BURST_BYTES).0, 30);
        // A GSO call carries 64 datagrams at most, and 60 kB.
        assert_eq!(
            next_chunk(None, t, [100; 80], 50e6, 1 << 20).0,
            MAX_SEGMENTS
        );
        assert_eq!(
            next_chunk(None, t, [mtu; 80], 50e6, 1 << 20).0,
            MAX_SEGMENTS_BYTES / mtu
        );
        // Paced at 50 MB/s, a packet every 28 µs: 8 go within the quantum.
        let (_, debt) = admit(None, t, BURST_BYTES + mtu, 50e6, BURST_BYTES);
        let (n, at, _) = next_chunk(Some(debt), t, [mtu; 40], 50e6, BURST_BYTES);
        assert!(at > t);
        assert!((7..=9).contains(&n), "{n} packets per quantum");
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
        let mut p = Pacer::new(1_000, 0);
        let r = p.pace(&packets, Duration::from_secs(1), |_| {
            n += 1;
            if n == 3 { Err("boom") } else { Ok(()) }
        });
        assert_eq!(r, Err("boom"));
        assert_eq!(n, 3);
    }
}
