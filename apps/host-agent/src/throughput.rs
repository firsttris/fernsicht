//! What a session's send thread sent, counted over a window of time: the
//! host's log line for our protocol, the overlay's numbers for a browser.

use std::time::{Duration, Instant};

use fernsicht_codec::EncodedFrame;

/// Frames sent in a window, with their bytes, packets and pipeline times.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Sent {
    pub frames: u64,
    pub bytes: u64,
    pub packets: u64,
    /// Sums over the frames: capture to captured, captured to encoded.
    pub capture_us: u64,
    pub encode_us: u64,
}

/// `sent` and one more frame of `bytes` in `packets`.
pub(crate) fn count(sent: Sent, enc: &EncodedFrame, bytes: u64, packets: u64) -> Sent {
    Sent {
        frames: sent.frames + 1,
        bytes: sent.bytes + bytes,
        packets: sent.packets + packets,
        capture_us: sent.capture_us + enc.capture_ready_us.saturating_sub(enc.capture_us),
        encode_us: sent.encode_us + enc.encoded_us.saturating_sub(enc.capture_ready_us),
    }
}

/// A window's counts per second, and the frames' average times.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rates {
    pub fps: f64,
    pub bits_per_sec: f64,
    pub packets_per_sec: f64,
    pub capture_us: u64,
    pub encode_us: u64,
}

/// What `sent` makes over `elapsed`.
pub(crate) fn rates(sent: Sent, elapsed: Duration) -> Rates {
    let secs = elapsed.as_secs_f64().max(f64::EPSILON);
    let frames = sent.frames.max(1);
    Rates {
        fps: sent.frames as f64 / secs,
        bits_per_sec: sent.bytes as f64 * 8.0 / secs,
        packets_per_sec: sent.packets as f64 / secs,
        capture_us: sent.capture_us / frames,
        encode_us: sent.encode_us / frames,
    }
}

/// A window of counting: when it started and what was sent since.
pub(crate) type Window = (Instant, Sent);

/// The window's rates once `period` has passed since it started; then a
/// new window starts at `now`.
pub(crate) fn close_after(window: &mut Window, now: Instant, period: Duration) -> Option<Rates> {
    let elapsed = now.saturating_duration_since(window.0);
    if elapsed < period {
        return None;
    }
    let r = rates(window.1, elapsed);
    *window = (now, Sent::default());
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(capture_us: u64, ready_us: u64, encoded_us: u64) -> EncodedFrame {
        EncodedFrame {
            capture_us,
            capture_ready_us: ready_us,
            encoded_us,
            ..EncodedFrame::default()
        }
    }

    #[test]
    fn frames_add_up_to_rates() {
        let sent = [frame(0, 1_000, 4_000), frame(10_000, 13_000, 14_000)]
            .iter()
            .fold(Sent::default(), |s, f| count(s, f, 125_000, 100));
        assert_eq!(
            sent,
            Sent {
                frames: 2,
                bytes: 250_000,
                packets: 200,
                capture_us: 4_000,
                encode_us: 4_000,
            }
        );
        let r = rates(sent, Duration::from_millis(500));
        assert_eq!(r.fps, 4.0);
        assert_eq!(r.bits_per_sec, 4_000_000.0);
        assert_eq!(r.packets_per_sec, 400.0);
        assert_eq!((r.capture_us, r.encode_us), (2_000, 2_000));
    }

    #[test]
    fn an_empty_window_has_no_rates_to_divide_by_zero() {
        let r = rates(Sent::default(), Duration::ZERO);
        assert_eq!((r.fps, r.bits_per_sec, r.capture_us), (0.0, 0.0, 0));
    }

    #[test]
    fn times_running_backwards_count_as_nothing() {
        let sent = count(Sent::default(), &frame(5_000, 4_000, 3_000), 1, 1);
        assert_eq!((sent.capture_us, sent.encode_us), (0, 0));
    }

    #[test]
    fn a_window_closes_after_its_period_and_starts_anew() {
        let start = Instant::now();
        let period = Duration::from_secs(1);
        let mut window: Window = (start, count(Sent::default(), &frame(0, 0, 0), 10, 1));
        assert_eq!(close_after(&mut window, start + period / 2, period), None);
        assert_eq!(window.1.frames, 1);
        let r = close_after(&mut window, start + period, period).unwrap();
        assert_eq!((r.fps, r.bits_per_sec), (1.0, 80.0));
        assert_eq!(window, (start + period, Sent::default()));
    }
}
