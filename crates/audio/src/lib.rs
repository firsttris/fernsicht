//! Audio streaming: what the host plays, to the client's speakers.
//!
//! - [`pulse`]: capture of the default output's monitor and playback,
//!   through PipeWire's PulseAudio interface.
//! - [`opus`]: Opus, 48 kHz stereo, 5 ms frames, low-delay mode.
//! - [`jitter`]: the client's jitter buffer.
//! - [`Tone`]: a test source (a sine), paced like real capture.
//!
//! The libraries (libopus, libpulse-simple) are loaded at runtime: builds
//! need nothing extra, and machines without them run without audio.

pub mod jitter;
pub mod opus;
pub mod pulse;

use std::time::{Duration, Instant};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME_MS: u32 = 5;
/// Samples per channel in one 5 ms frame.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE * FRAME_MS / 1000) as usize;

/// One frame of interleaved 16-bit stereo.
pub type Frame = [i16; FRAME_SAMPLES * CHANNELS];

pub trait AudioSource: Send {
    /// Blocks until a frame is filled. Returns when its sound was produced
    /// (µs, local monotonic clock).
    fn next_frame(&mut self, pcm: &mut Frame) -> Result<u64, String>;
}

pub trait AudioSink: Send {
    /// Queues a frame for playback; blocks while the output buffer is full,
    /// which paces the caller.
    fn play(&mut self, pcm: &Frame) -> Result<(), String>;

    /// Audio queued in the output, µs.
    fn buffered_us(&mut self) -> u64;
}

/// A sine on both channels, paced at the frame rate like real capture.
pub struct Tone {
    hz: f64,
    phase: u64,
    next: Option<Instant>,
}

impl Tone {
    pub fn new(hz: f64) -> Self {
        Self {
            hz,
            phase: 0,
            next: None,
        }
    }
}

impl AudioSource for Tone {
    fn next_frame(&mut self, pcm: &mut Frame) -> Result<u64, String> {
        let due = *self.next.get_or_insert_with(Instant::now);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        self.next = Some(due + Duration::from_millis(u64::from(FRAME_MS)));
        for s in 0..FRAME_SAMPLES {
            let t = (self.phase + s as u64) as f64 / f64::from(SAMPLE_RATE);
            let v = (t * self.hz * std::f64::consts::TAU).sin() * 8000.0;
            pcm[2 * s] = v as i16;
            pcm[2 * s + 1] = v as i16;
        }
        self.phase += FRAME_SAMPLES as u64;
        Ok(fernsicht_core::now_us())
    }
}

/// Keeps what would be played (tests), paced like a sound card: one frame
/// per 5 ms on a fixed grid.
#[derive(Clone, Default)]
pub struct Recorder {
    pub pcm: std::sync::Arc<std::sync::Mutex<Vec<i16>>>,
    next: Option<Instant>,
}

impl Recorder {
    /// Everything played so far.
    pub fn played(&self) -> Vec<i16> {
        self.pcm.lock().map(|p| p.clone()).unwrap_or_default()
    }
}

impl AudioSink for Recorder {
    fn play(&mut self, pcm: &Frame) -> Result<(), String> {
        self.pcm
            .lock()
            .map_err(|e| e.to_string())?
            .extend_from_slice(pcm);
        let due = *self.next.get_or_insert_with(Instant::now)
            + Duration::from_millis(u64::from(FRAME_MS));
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        self.next = Some(due);
        Ok(())
    }

    fn buffered_us(&mut self) -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_keeps_the_sound_cards_pace() {
        let mut r = Recorder::default();
        let frame = [1i16; FRAME_SAMPLES * CHANNELS];
        let start = Instant::now();
        for _ in 0..40 {
            r.play(&frame).unwrap();
        }
        let took = start.elapsed();
        // 40 × 5 ms on a fixed grid: no drift from sleeping too long.
        assert!(
            took >= Duration::from_millis(195) && took < Duration::from_millis(230),
            "{took:?}"
        );
        assert_eq!(r.played().len(), 40 * frame.len());
    }

    #[test]
    fn frame_size() {
        assert_eq!(FRAME_SAMPLES, 240);
        assert_eq!(std::mem::size_of::<Frame>(), 960);
    }

    #[test]
    fn tone_is_a_continuous_sine_at_the_frame_rate() {
        let mut t = Tone::new(1000.0);
        let mut a = [0i16; FRAME_SAMPLES * CHANNELS];
        let mut b = a;
        let start = Instant::now();
        t.next_frame(&mut a).unwrap();
        for _ in 0..4 {
            t.next_frame(&mut b).unwrap();
        }
        assert!(start.elapsed() >= Duration::from_millis(19), "paced");
        // 1 kHz at 48 kHz: period 48 samples; a frame is 5 periods, so
        // every frame starts the same (up to rounding).
        assert!(a.iter().zip(&b).all(|(x, y)| (x - y).abs() <= 1));
        assert_eq!(a[0], 0);
        assert!(a.iter().any(|&v| v > 7900));
        assert_eq!(a[2 * 12], a[2 * 12 + 1], "both channels");
    }
}
