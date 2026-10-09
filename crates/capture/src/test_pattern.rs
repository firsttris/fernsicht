use std::time::{Duration, Instant};

use fernsicht_core::{clock, now_us};

use crate::{CaptureError, Frame, FrameSource, PixelFormat};

/// NV12 test source: grey background with a white vertical bar moving one
/// bar width per frame, paced to the requested frame rate.
pub struct TestPattern {
    width: u32,
    height: u32,
    interval: Duration,
    next_due: Option<Instant>,
    seq: u64,
    bar_width: u32,
}

impl TestPattern {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        let width = width.max(2) & !1;
        let height = height.max(2) & !1;
        Self {
            width,
            height,
            interval: Duration::from_micros(clock::frame_interval_us(fps)),
            next_due: None,
            seq: 0,
            bar_width: (width / 32).max(2),
        }
    }

    fn bar_x(&self, seq: u64) -> u32 {
        let positions = u64::from(self.width / self.bar_width);
        (seq % positions) as u32 * self.bar_width
    }

    fn draw(&self, frame: &mut Frame, seq: u64) {
        let (w, h) = (self.width as usize, self.height as usize);
        let luma = &mut frame.data[..w * h];
        // A frame buffer may come back from the pipeline with any older
        // picture in it, so redraw the luma plane fully. memset-speed.
        luma.fill(64);
        let x = self.bar_x(seq) as usize;
        let bw = self.bar_width as usize;
        for row in luma.chunks_exact_mut(w) {
            row[x..x + bw].fill(235);
        }
        frame.data[w * h..].fill(128);
    }
}

impl FrameSource for TestPattern {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn format(&self) -> PixelFormat {
        PixelFormat::Nv12
    }

    fn next_frame(&mut self, frame: &mut Frame) -> Result<(), CaptureError> {
        if frame.width != self.width
            || frame.height != self.height
            || frame.format != PixelFormat::Nv12
        {
            return Err(CaptureError::FormatMismatch);
        }
        let now = Instant::now();
        let due = *self.next_due.get_or_insert(now);
        if due > now {
            std::thread::sleep(due - now);
        }
        // Stay on the original grid; if we fell behind, skip ahead instead
        // of bursting.
        let mut next = due + self.interval;
        let now = Instant::now();
        while next <= now {
            next += self.interval;
        }
        self.next_due = Some(next);

        frame.capture_us = now_us();
        self.draw(frame, self.seq);
        frame.seq = self.seq;
        frame.ready_us = now_us();
        self.seq += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_paced_frames() {
        let mut src = TestPattern::new(64, 32, 200);
        let mut f = src.alloc_frame();
        let start = Instant::now();
        for i in 0..5 {
            src.next_frame(&mut f).unwrap();
            assert_eq!(f.seq, i);
            assert!(f.ready_us >= f.capture_us);
        }
        // 4 intervals of 5 ms after the first frame
        assert!(start.elapsed() >= Duration::from_millis(19));
    }

    #[test]
    fn bar_moves() {
        let mut src = TestPattern::new(64, 4, 1000);
        let mut f = src.alloc_frame();
        src.next_frame(&mut f).unwrap();
        let first = f.data[..64].to_vec();
        src.next_frame(&mut f).unwrap();
        assert_ne!(first, f.data[..64].to_vec());
        assert_eq!(f.data.len(), 64 * 4 + 64 * 2);
    }

    #[test]
    fn rejects_wrong_buffer() {
        let mut src = TestPattern::new(64, 32, 60);
        let mut f = Frame::new(32, 32, PixelFormat::Nv12);
        assert!(matches!(
            src.next_frame(&mut f),
            Err(CaptureError::FormatMismatch)
        ));
    }
}
