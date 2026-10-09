//! Audio streaming.
//!
//! Phase 2: capture the PipeWire monitor source of the default sink, encode
//! Opus in 5 ms frames, send on its own stream, sync to video on the client.

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME_MS: u32 = 5;
/// Samples per channel in one 5 ms frame.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE * FRAME_MS / 1000) as usize;

pub trait AudioSource: Send {
    /// Blocks until `FRAME_SAMPLES` interleaved stereo samples are filled.
    /// Returns the capture timestamp (µs, local monotonic clock).
    fn next_frame(&mut self, pcm: &mut [i16; FRAME_SAMPLES * CHANNELS]) -> Result<u64, String>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn frame_size() {
        assert_eq!(super::FRAME_SAMPLES, 240);
    }
}
