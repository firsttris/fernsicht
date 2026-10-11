//! Vulkan on the GPU that does the video work: device and queue, and
//! DMA-BUFs imported as images without a copy.
//!
//! Used by the client's renderer (decoded pictures → screen) and by the
//! host's NVIDIA encoder (captured screen → NV12 for NVENC).

mod device;
pub mod import;

pub use device::{
    DEVICE_ENV, Features, GPU_TIMEOUT, Gpu, Prefer, QueueGuard, QueueLock, Timeline, VideoQueue,
    submit, submit_and_wait, submit_timeline, submit_with, wait_fence,
};
pub mod convert;
pub mod testimage;
