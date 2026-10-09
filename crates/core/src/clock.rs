//! Process-local monotonic clock in microseconds.
//!
//! The epoch is arbitrary (first use in this process). Host and client
//! clocks are related through the NTP-style offset estimation in
//! `fernsicht-net` (`clock_sync`), so only monotonicity matters here.

use std::sync::OnceLock;
use std::time::Instant;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Microseconds since this process first asked for the time.
#[inline]
pub fn now_us() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// Converts a frame rate into the frame interval in microseconds.
#[inline]
pub fn frame_interval_us(fps: u32) -> u64 {
    1_000_000 / u64::from(fps.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic() {
        let a = now_us();
        let b = now_us();
        assert!(b >= a);
    }

    #[test]
    fn interval() {
        assert_eq!(frame_interval_us(60), 16_666);
        assert_eq!(frame_interval_us(0), 1_000_000);
    }
}
