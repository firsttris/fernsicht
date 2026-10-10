//! Vulkan on the GPU that does the video work: device and queue, and
//! DMA-BUFs imported as images without a copy.
//!
//! Used by the client's renderer (decoded pictures → screen) and by the
//! host's NVIDIA encoder (captured screen → NV12 for NVENC).

mod device;
pub mod import;

pub use device::{DEVICE_ENV, Gpu};
pub mod convert;
pub mod testimage;
