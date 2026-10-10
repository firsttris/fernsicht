//! Vulkan presentation: NV12 pictures from CPU memory or straight from the
//! decoder's DMA-BUF, converted to RGB by a shader, into an offscreen image
//! (tests) or a window's swapchain.

mod gpu;
mod import;
mod renderer;
#[cfg(feature = "window")]
pub mod window;

pub use gpu::{DEVICE_ENV, Gpu};
pub use renderer::{RenderError, Renderer, letterbox, spirv};
