//! Client-side presentation.
//!
//! - [`vulkan`] (feature `vulkan`): converts NV12 pictures to RGB on the
//!   GPU, from CPU memory or straight from the decoder's DMA-BUF.
//! - [`vulkan::window`] (feature `window`): the native video window,
//!   Mailbox/Immediate present.
//! - [`HeadlessPresenter`]: discards frames, so the pipeline and the
//!   latency overlay run anywhere (CI, `--headless`).

pub mod overlay;
#[cfg(feature = "vulkan")]
pub mod vulkan;

use std::sync::Arc;

use fernsicht_capture::CursorImage;
use fernsicht_codec::{DecodedFrame, Picture, PictureKind};

/// The pointer to draw over the video.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorOverlay {
    /// Changes with the image (presenters cache the uploaded texture).
    pub serial: u32,
    /// BGRA, premultiplied alpha.
    pub image: Arc<CursorImage>,
    /// Top-left on the host's screen, in its pixels.
    pub x: i32,
    pub y: i32,
    /// The host's screen size; the video may be scaled from it.
    pub screen_width: u32,
    pub screen_height: u32,
}

pub trait Presenter {
    /// The picture form wanted from the decoder; `None` needs no picture.
    fn wants(&self) -> Option<PictureKind> {
        None
    }

    /// Shows `frame`; returns once it has been handed to the display.
    /// `picture` is `None` for codecs without pictures (synthetic) or when
    /// [`Self::wants`] is `None`.
    fn present(
        &mut self,
        frame: &DecodedFrame,
        picture: Option<&Picture<'_>>,
        cursor: Option<&CursorOverlay>,
    ) -> Result<(), String>;

    /// The latency overlay, about once per second.
    fn overlay(&mut self, _lines: &[String]) {}
}

/// The picture's size relative to a target of `dst` pixels when `src` is
/// shown as large as possible without distortion (1.0 = full width or
/// height); the rest are black bars.
pub fn letterbox_size(src: (u32, u32), dst: (u32, u32)) -> [f32; 2] {
    let (sw, sh) = (src.0.max(1) as f32, src.1.max(1) as f32);
    let (dw, dh) = (dst.0.max(1) as f32, dst.1.max(1) as f32);
    let s = (dw / sw).min(dh / sh);
    [sw * s / dw, sh * s / dh]
}

/// Discards frames. Presentation time is then just the hand-off cost.
#[derive(Default)]
pub struct HeadlessPresenter {
    pub presented: u64,
}

impl Presenter for HeadlessPresenter {
    fn present(
        &mut self,
        _frame: &DecodedFrame,
        _picture: Option<&Picture<'_>>,
        _cursor: Option<&CursorOverlay>,
    ) -> Result<(), String> {
        self.presented += 1;
        Ok(())
    }
}
