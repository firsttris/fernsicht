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

use fernsicht_codec::{DecodedFrame, Picture, PictureKind};

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
    ) -> Result<(), String>;

    /// The latency overlay, about once per second.
    fn overlay(&mut self, _lines: &[String]) {}
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
    ) -> Result<(), String> {
        self.presented += 1;
        Ok(())
    }
}
