//! Screen capture.
//!
//! A [`FrameSource`] fills a caller-owned [`Frame`], so buffers are
//! allocated once and recycled through the pipeline slots.
//!
//! Backends:
//! - [`TestPattern`]: synthetic moving bar at a fixed frame rate, runs
//!   anywhere (CI, containers) and drives the pipeline end to end.
//! - KMS/DRM (planned, phase 1): framebuffer plane → DMA-BUF, needs
//!   `CAP_SYS_ADMIN`, works on the login screen and unattended.
//! - PipeWire via xdg-desktop-portal ScreenCast (planned, phase 1):
//!   DMA-BUF frames, user confirmation, restore token.

mod test_pattern;

pub use test_pattern::TestPattern;

use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit B, G, R, unused — what KMS and PipeWire usually hand out.
    Bgrx,
    /// Y plane followed by interleaved UV at half resolution — encoder input.
    Nv12,
}

impl PixelFormat {
    pub fn frame_bytes(self, width: u32, height: u32) -> usize {
        let (w, h) = (width as usize, height as usize);
        match self {
            PixelFormat::Bgrx => w * h * 4,
            PixelFormat::Nv12 => w * h + w * h.div_ceil(2),
        }
    }
}

/// One captured frame in CPU memory. DMA-BUF frames get their own variant
/// once the GPU backends exist.
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    /// Source sequence number, increments per captured frame.
    pub seq: u64,
    /// Local monotonic clock (µs): when the image was produced (vblank).
    pub capture_us: u64,
    /// Local monotonic clock (µs): when the frame was available to us.
    pub ready_us: u64,
}

impl Frame {
    pub fn new(width: u32, height: u32, format: PixelFormat) -> Self {
        Self {
            width,
            height,
            format,
            data: vec![0; format.frame_bytes(width, height)],
            seq: 0,
            capture_us: 0,
            ready_us: 0,
        }
    }
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("capture source stopped")]
    Stopped,
    #[error("frame buffer does not match source format")]
    FormatMismatch,
    #[error("capture backend: {0}")]
    Backend(String),
}

pub trait FrameSource: Send {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn format(&self) -> PixelFormat;

    /// Blocks until the next frame is available and writes it into `frame`.
    fn next_frame(&mut self, frame: &mut Frame) -> Result<(), CaptureError>;

    /// A frame buffer matching this source.
    fn alloc_frame(&self) -> Frame {
        Frame::new(self.width(), self.height(), self.format())
    }
}
