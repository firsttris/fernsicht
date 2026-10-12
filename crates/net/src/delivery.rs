//! How fast the path delivers (C3): the rate the host can pace at without
//! filling a queue on the way.
//!
//! A frame that fits in the pacer's burst ([`BURST_BYTES`]) leaves the host
//! back-to-back, at its NIC's rate; it arrives spread out by the slowest
//! link on the way. Its bytes after the first packet over the time from
//! the first to the last packet are that link's rate. A frame that arrives
//! faster than [`MIN_SPAN_US`] says little (the kernel hands a GRO run
//! over at once), and one with retransmissions nothing; both are left out.
//! An interval reports the median of its frames, so that a frame a
//! scheduler held up does not pull the rate down.
//!
//! [`BURST_BYTES`]: crate::pacer::BURST_BYTES

use crate::pacer::BURST_BYTES;

/// Frames that arrived within this, µs, are not measured.
pub const MIN_SPAN_US: u64 = 250;

/// Samples an interval keeps; at 240 fps a 100 ms interval brings 24, the
/// first 32 stand for it.
const SAMPLES: usize = 32;

#[derive(Debug)]
pub struct DeliveryRate {
    /// The interval's samples, kbit/s; fixed, so a frame costs no
    /// allocation.
    kbps: [u32; SAMPLES],
    count: usize,
}

impl Default for DeliveryRate {
    fn default() -> Self {
        Self {
            kbps: [0; SAMPLES],
            count: 0,
        }
    }
}

/// One completed frame, as the reassembler saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arrival {
    /// Bytes and packets until it was complete.
    pub bytes: usize,
    pub packets: usize,
    /// Client clock, µs: its first packet, the one that completed it.
    pub first_us: u64,
    pub completed_us: u64,
    pub retransmitted: bool,
}

/// The rate a frame arrived at, kbit/s; `None` when it does not measure
/// the path.
pub fn sample(a: &Arrival) -> Option<u32> {
    let span = a.completed_us.saturating_sub(a.first_us);
    if a.retransmitted || a.packets < 2 || a.bytes > BURST_BYTES || span < MIN_SPAN_US {
        return None;
    }
    // The first packet's bytes arrived before the span began.
    let after_first = (a.bytes - a.bytes / a.packets) as u64;
    // bytes · 8 / µs = Mbit/s; · 1000 = kbit/s.
    u32::try_from(after_first * 8_000 / span).ok()
}

impl DeliveryRate {
    pub fn push(&mut self, a: &Arrival) {
        if let Some(kbps) = sample(a)
            && self.count < SAMPLES
        {
            self.kbps[self.count] = kbps;
            self.count += 1;
        }
    }

    /// The interval's median, kbit/s (0: nothing measured); starts the
    /// next interval.
    pub fn take_interval(&mut self) -> u32 {
        let samples = &mut self.kbps[..self.count];
        self.count = 0;
        if samples.is_empty() {
            return 0;
        }
        samples.sort_unstable();
        samples[samples.len() / 2]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bytes: usize, packets: usize, span_us: u64) -> Arrival {
        Arrival {
            bytes,
            packets,
            first_us: 1_000_000,
            completed_us: 1_000_000 + span_us,
            retransmitted: false,
        }
    }

    #[test]
    fn a_burst_spread_by_the_slowest_link_measures_it() {
        // 40 packets of 1250 bytes through 100 Mbit/s: 100 µs apart.
        let a = frame(50_000, 40, 39 * 100);
        assert_eq!(sample(&a), Some(100_000));
    }

    #[test]
    fn what_says_nothing_about_the_path_is_left_out() {
        assert_eq!(sample(&frame(50_000, 40, 100)), None, "one GRO run");
        assert_eq!(sample(&frame(1_200, 1, 900)), None, "one packet");
        let paced = frame(BURST_BYTES + 1, 60, 5_000);
        assert_eq!(sample(&paced), None, "the pacer spread it, not the path");
        let resent = Arrival {
            retransmitted: true,
            ..frame(50_000, 40, 3_900)
        };
        assert_eq!(sample(&resent), None);
    }

    #[test]
    fn an_interval_reports_its_median() {
        let mut d = DeliveryRate::default();
        assert_eq!(d.take_interval(), 0, "nothing measured");
        // Five frames at 100 Mbit/s, one a scheduler held up, one that
        // came at once.
        for _ in 0..5 {
            d.push(&frame(50_000, 40, 3_900));
        }
        d.push(&frame(50_000, 40, 39_000));
        d.push(&frame(50_000, 40, 50));
        assert_eq!(d.take_interval(), 100_000);
        assert_eq!(d.take_interval(), 0, "the next interval starts empty");
        // More frames than samples: the first stand for the interval.
        for _ in 0..SAMPLES + 10 {
            d.push(&frame(50_000, 40, 3_900));
        }
        assert_eq!(d.take_interval(), 100_000);
    }
}
