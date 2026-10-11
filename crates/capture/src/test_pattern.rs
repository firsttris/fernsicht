use std::time::{Duration, Instant};

use fernsicht_core::{clock, now_us};

use std::sync::Arc;

use crate::{CaptureError, CursorImage, CursorState, Frame, FrameSource, PixelFormat};

/// NV12 test source: grey background with a white vertical bar moving one
/// bar width per frame, paced to the requested frame rate. Reports a
/// pointer (an arrow) circling the middle of the screen, once per 2 s.
pub struct TestPattern {
    width: u32,
    height: u32,
    interval: Duration,
    next_due: Option<Instant>,
    seq: u64,
    bar_width: u32,
    arrow: Arc<CursorImage>,
    /// Frame slots in which the "monitor" is off and capture fails, as KMS
    /// does for a monitor in standby (tests).
    outage: std::ops::Range<u64>,
    /// Frame slots in which the picture stands still, as a desktop nobody
    /// touches (tests).
    still: std::ops::Range<u64>,
    /// The slot of the last frame drawn: the pointer belongs to it.
    shown: Option<u64>,
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
            arrow: Arc::new(CursorImage::arrow()),
            outage: 0..0,
            still: 0..0,
            shown: None,
        }
    }

    /// The same pattern, but capture fails in the frame slots `off` (on the
    /// same frame grid), like a monitor that goes to standby and wakes up.
    pub fn with_outage(self, off: std::ops::Range<u64>) -> Self {
        Self {
            outage: off,
            ..self
        }
    }

    /// The same pattern, but standing still in the frame slots `still`
    /// (the frames after its first say they did not change).
    pub fn with_still(self, still: std::ops::Range<u64>) -> Self {
        Self { still, ..self }
    }

    /// Where the pointer is in frame `seq`: on a circle, 120 frames a turn.
    pub fn cursor_at(&self, seq: u64) -> CursorState {
        let angle = (seq % 120) as f64 / 120.0 * std::f64::consts::TAU;
        let r = f64::from(self.height) / 4.0;
        CursorState {
            visible: true,
            x: (f64::from(self.width) / 2.0 + r * angle.cos()).round() as i32,
            y: (f64::from(self.height) / 2.0 + r * angle.sin()).round() as i32,
            serial: 1,
            image: self.arrow.clone(),
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

    fn cursor(&mut self) -> Option<CursorState> {
        self.shown.map(|seq| self.cursor_at(seq))
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
        // of bursting (and the pattern with it: `seq` counts grid slots).
        let mut next = due + self.interval;
        let mut skipped = 0;
        let now = Instant::now();
        while next <= now {
            next += self.interval;
            skipped += 1;
        }
        self.next_due = Some(next);
        if self.outage.contains(&self.seq) {
            self.seq += 1 + skipped;
            return Err(CaptureError::Backend("no framebuffer (monitor off)".into()));
        }

        frame.capture_us = now_us();
        // A still stretch shows the picture of its first slot throughout.
        let still = self.still.contains(&self.seq);
        frame.changed = !(still && self.shown.is_some_and(|s| self.still.contains(&s)));
        self.draw(frame, if still { self.still.start } else { self.seq });
        frame.seq = self.seq;
        self.shown = Some(self.seq);
        frame.ready_us = now_us();
        self.seq += 1 + skipped;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outage_fails_its_frames_and_ends() {
        let mut src = TestPattern::new(64, 32, 100).with_outage(2..4);
        let mut frame = src.alloc_frame();
        let results: Vec<bool> = (0..6).map(|_| src.next_frame(&mut frame).is_ok()).collect();
        assert_eq!(results, [true, true, false, false, true, true]);
        // The pattern keeps its place: the bar moved on during the outage.
        assert_eq!(frame.seq, 5);
    }

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
    fn pointer_circles_the_middle() {
        let src = TestPattern::new(640, 480, 60);
        let a = src.cursor_at(0);
        assert_eq!((a.x, a.y), (320 + 120, 240));
        let b = src.cursor_at(30);
        assert_eq!((b.x, b.y), (320, 240 + 120));
        assert_eq!(src.cursor_at(120), a, "a turn is 120 frames");
        assert!(a.visible && a.serial == 1 && a.image.width == 12);

        let mut src = TestPattern::new(64, 32, 1000);
        assert!(src.cursor().is_none(), "no pointer before a frame");
        let mut f = src.alloc_frame();
        src.next_frame(&mut f).unwrap();
        assert_eq!(
            src.cursor(),
            Some(src.cursor_at(f.seq)),
            "the frame's pointer"
        );
    }

    #[test]
    fn a_still_stretch_says_it_did_not_change() {
        let mut src = TestPattern::new(64, 32, 1000).with_still(2..5);
        let mut f = src.alloc_frame();
        let mut seen = Vec::new();
        for _ in 0..6 {
            src.next_frame(&mut f).unwrap();
            seen.push((f.seq, f.changed, f.data.clone()));
        }
        let changed: Vec<bool> = seen.iter().map(|(_, c, _)| *c).collect();
        // Slots skipped under load shift the pattern; compare by slot.
        for (seq, c, data) in &seen {
            let in_still = (3..5).contains(seq);
            assert_eq!(*c, !in_still, "slot {seq}: {changed:?}");
            if in_still {
                let first = seen.iter().find(|(s, _, _)| *s == 2);
                if let Some((_, _, d)) = first {
                    assert_eq!(d, data, "slot {seq} shows slot 2's picture");
                }
            }
        }
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
