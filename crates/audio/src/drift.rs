//! Two sound cards never run at exactly the same speed: the host's
//! produces a little more or less sound than the client's plays (up to
//! about 100 ppm), and the buffer between them grows or drains. Instead of
//! dropping a whole frame (a click) or concealing one after running dry (a
//! gap), the client plays a frame one sample shorter or longer now and
//! then: 0.4 % for 5 ms, far below what anyone hears.

/// How much one measurement moves the smoothed delay: a time constant of
/// about half a second at 200 frames a second, so network jitter averages
/// out.
const SMOOTHING: f64 = 0.01;

/// How far the smoothed delay may stray from the target before frames are
/// stretched: about one frame either way.
const SLACK_US: f64 = 5_000.0;

/// Keeps the audio delay near a target.
#[derive(Clone, Copy, Debug)]
pub struct DriftControl {
    target_us: f64,
    smoothed_us: Option<f64>,
}

impl DriftControl {
    pub fn new(target_us: u64) -> Self {
        Self {
            target_us: target_us as f64,
            smoothed_us: None,
        }
    }

    /// One measurement of the sound waiting to be played (µs), before a
    /// frame is played: how many samples per channel that frame should get
    /// more (+1: the buffer drains, play slower) or fewer (−1: it grows).
    pub fn step(&mut self, delay_us: u64) -> i32 {
        let d = delay_us as f64;
        let s = self.smoothed_us.map_or(d, |s| s + (d - s) * SMOOTHING);
        self.smoothed_us = Some(s);
        if s > self.target_us + SLACK_US {
            -1
        } else if s < self.target_us - SLACK_US {
            1
        } else {
            0
        }
    }
}

/// `frame` (interleaved, `channels` per sample) resampled to `delta` more
/// samples per channel by linear interpolation, into `out` (cleared
/// first). The first and last samples stay where they are, so frames join
/// without a step.
pub fn stretch(frame: &[i16], channels: usize, delta: i32, out: &mut Vec<i16>) {
    out.clear();
    let n = frame.len() / channels.max(1);
    let m = (n as i64 + i64::from(delta)).max(1) as usize;
    if delta == 0 || n < 2 {
        out.extend_from_slice(frame);
        return;
    }
    let scale = (n - 1) as f64 / (m - 1).max(1) as f64;
    for i in 0..m {
        let pos = i as f64 * scale;
        let left = (pos.floor() as usize).min(n - 1);
        let right = (left + 1).min(n - 1);
        let t = pos - left as f64;
        for c in 0..channels {
            let a = f64::from(frame[left * channels + c]);
            let b = f64::from(frame[right * channels + c]);
            out.push((a + (b - a) * t).round() as i16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_get_one_sample_more_or_less() {
        // A ramp on both channels (the right one negative).
        let frame: Vec<i16> = (0..240i16).flat_map(|i| [i * 10, -i * 10]).collect();
        let mut out = Vec::new();
        stretch(&frame, 2, 0, &mut out);
        assert_eq!(out, frame);
        for delta in [-1, 1] {
            stretch(&frame, 2, delta, &mut out);
            assert_eq!(out.len(), (240 + delta) as usize * 2);
            // The ends stay, the ramp stays a ramp.
            assert_eq!(&out[..2], &[0, 0]);
            assert_eq!(&out[out.len() - 2..], &[2390, -2390]);
            let left: Vec<i16> = out.iter().step_by(2).copied().collect();
            assert!(left.windows(2).all(|w| w[1] >= w[0]));
        }
    }

    #[test]
    fn the_delay_is_steered_only_when_it_strays() {
        let mut c = DriftControl::new(15_000);
        assert_eq!(c.step(15_000), 0);
        // One late burst does not count: it is smoothed.
        assert_eq!(c.step(40_000), 0);
        // A buffer that stays large: shorter frames.
        let mut c = DriftControl::new(15_000);
        let last = (0..500).map(|_| c.step(30_000)).last();
        assert_eq!(last, Some(-1));
        let mut c = DriftControl::new(15_000);
        let last = (0..500).map(|_| c.step(2_000)).last();
        assert_eq!(last, Some(1));
    }

    /// A host clock 300 ppm off either way, for ten minutes of sound: the
    /// buffer stays within a frame or two of the target, and is never
    /// dropped or run dry.
    #[test]
    fn clock_drift_is_absorbed() {
        for ppm in [-300.0, 300.0] {
            let mut c = DriftControl::new(15_000);
            // Samples per channel waiting, starting at the target.
            let mut waiting = 15.0 * 48.0;
            let (mut low, mut high) = (f64::MAX, f64::MIN);
            for _ in 0..120_000 {
                // Each frame takes one frame of input and plays for its
                // stretched length; meanwhile the host's clock delivers.
                let us = (waiting / 48.0 * 1000.0) as u64;
                let played = f64::from(240 + c.step(us));
                waiting += played * (1.0 + ppm / 1e6) - 240.0;
                low = low.min(waiting);
                high = high.max(waiting);
            }
            let (low_ms, high_ms) = (low / 48.0, high / 48.0);
            assert!(
                low_ms > 5.0 && high_ms < 25.0,
                "{ppm} ppm: {low_ms:.1}–{high_ms:.1} ms"
            );
        }
    }
}
