//! Client-side presentation.
//!
//! The native video window (phase 1: `winit` + Vulkan via `ash`, VAAPI
//! decode straight into a texture, Mailbox/Immediate present) implements
//! [`Presenter`]. Until then [`HeadlessPresenter`] stands in so the full
//! pipeline and the latency overlay run anywhere.

pub mod overlay;

use fernsicht_codec::DecodedFrame;

pub trait Presenter {
    /// Shows `frame`; returns once it has been handed to the display.
    fn present(&mut self, frame: &DecodedFrame) -> Result<(), String>;
}

/// Discards frames. Presentation time is then just the hand-off cost.
#[derive(Default)]
pub struct HeadlessPresenter {
    pub presented: u64,
}

impl Presenter for HeadlessPresenter {
    fn present(&mut self, _frame: &DecodedFrame) -> Result<(), String> {
        self.presented += 1;
        Ok(())
    }
}
