//! Video encode and decode.
//!
//! Backends:
//! - [`synthetic`]: produces a checksummed payload of realistic size for the
//!   configured bitrate. No real compression; it lets transport, FEC and
//!   latency measurement run on machines without a GPU.
//! - [`vaapi`] (feature `vaapi`): AV1, HEVC and H.264 low-latency encode
//!   and decode in hardware via FFmpeg (radeonsi on the RX 7800 XT, iHD on
//!   Intel). Takes KMS DMA-BUFs without a copy.
//! - [`nvidia`] (feature `nvidia`): H.264, HEVC and AV1 with NVENC and
//!   NVDEC via FFmpeg (GTX 1080 and newer); pictures stay on the GPU (screen
//!   input through Vulkan, decoded pictures exported from CUDA).
//! - [`vulkan`] (feature `vulkan`): H.264, HEVC and AV1 decode with Vulkan
//!   Video via FFmpeg, on the presenter's own Vulkan device: any GPU whose
//!   driver has it, and the pictures are sampled where they were decoded.

#[cfg(feature = "nvidia")]
mod cuda;
#[cfg(any(feature = "vaapi", feature = "nvidia", feature = "vulkan"))]
mod ff;
#[cfg(feature = "nvidia")]
pub mod nvidia;
pub mod synthetic;
#[cfg(feature = "vaapi")]
pub mod vaapi;
#[cfg(feature = "vulkan")]
pub mod vulkan;

use std::os::fd::BorrowedFd;

use fernsicht_capture::{DmaBuf, Frame};
use fernsicht_core::colour::Colour;
use fernsicht_proto::Codec;
use thiserror::Error;

/// Encoder output. The buffer is reused across frames.
#[derive(Clone, Debug, Default)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Position in the encoded stream: consecutive, starting at 0. A gap
    /// downstream means a frame the decoder needs went missing.
    pub frame_id: u32,
    /// Capture sequence number of the source frame.
    pub seq: u64,
    pub capture_us: u64,
    pub capture_ready_us: u64,
    pub encoded_us: u64,
    /// The picture's size (set by the host's encode loop).
    pub width: u16,
    pub height: u16,
}

/// What a decoder says about a decoded picture; the picture itself stays
/// on the GPU and is handed out by `Decoder::picture`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub seq: u64,
    pub keyframe: bool,
    /// What the stream says about its colour (depth, primaries, transfer).
    pub colour: Colour,
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("corrupt bitstream: {0}")]
    Corrupt(&'static str),
    #[error("waiting for a keyframe")]
    NeedKeyframe,
    #[error("codec backend: {0}")]
    Backend(String),
    /// The GPU stopped finishing work and was given up: nothing more will
    /// decode or encode on it.
    #[error("the GPU stopped responding and was given up")]
    GpuLost,
}

pub trait Encoder: Send {
    fn codec(&self) -> Codec;

    fn encode(&mut self, frame: &Frame, out: &mut EncodedFrame) -> Result<(), CodecError>;

    /// The next frame will be a keyframe (or start an intra refresh).
    fn request_keyframe(&mut self);

    /// Changes the bitrate of the running encoder. It keeps its GPU objects
    /// (device, frame pool, screen input); the hardware encoders restart
    /// their rate control with a keyframe. On an error the old rate stays.
    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError>;

    /// The stream's colour (written into it): its depth, primaries and
    /// transfer.
    fn colour(&self) -> fernsicht_core::colour::Colour {
        fernsicht_core::colour::Colour::SDR
    }
}

/// The picture of the last decoded frame, for the presenter. Each says
/// how its values are to be read ([`Colour`]): limited range, the matrix
/// of its primaries; at 10 bit the samples are P010's (16 bits, the 10 on
/// top).
#[derive(Debug)]
pub enum Picture<'a> {
    /// Tightly packed NV12 in CPU memory, or P010 at 10 bit.
    Nv12 {
        width: u32,
        height: u32,
        data: &'a [u8],
        colour: Colour,
    },
    /// 4:2:0 on the GPU (fourcc `NV12` or `P010`, plane 0 Y, plane 1
    /// interleaved UV).
    /// `key` stays the same for the same underlying surface, so presenters
    /// can keep the import instead of repeating it every frame. Its upper
    /// 32 bits name the surface pool: a new value means the pool was
    /// replaced (a new decoder, a new stream size), and imports of older
    /// pools are stale.
    DmaBuf {
        image: &'a DmaBuf,
        key: u64,
        colour: Colour,
    },
    /// NV12 in GPU memory CUDA exported (NVDEC's pictures): import `memory`
    /// as Vulkan memory (`VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT`,
    /// `size` bytes, on the GPU with `device_uuid`) and copy the planes
    /// out. CUDA has finished writing; the memory holds this picture until
    /// the next [`Decoder::picture`]. `id` changes when the memory does:
    /// an import of another id is stale.
    Exported {
        memory: BorrowedFd<'a>,
        size: u64,
        id: u64,
        device_uuid: [u8; 16],
        width: u32,
        height: u32,
        /// Byte offset and row pitch of the Y and the interleaved UV plane.
        planes: [(u64, u32); 2],
        colour: Colour,
    },
    /// A Vulkan image on the presenter's own device (FFmpeg's Vulkan
    /// decoder): multi-planar 4:2:0 (`G8_B8R8_2PLANE_420_UNORM`, or
    /// `G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16` at 10 bit, created with
    /// `MUTABLE_FORMAT`, so each plane can be viewed on its own), sampled
    /// where it was decoded.
    Vulkan(VkPicture<'a>),
}

/// A decoded picture in a Vulkan image; see [`Picture::Vulkan`]. Handles
/// are raw (`ash`'s `from_raw`).
#[derive(Clone, Copy)]
pub struct VkPicture<'a> {
    pub image: u64,
    /// The image's size; the picture is the top-left `width`×`height`.
    pub coded: (u32, u32),
    pub width: u32,
    pub height: u32,
    /// Changes when the decoder's images do (a new decoder, a new size):
    /// views of images of another pool are stale.
    pub pool: u64,
    pub colour: Colour,
    pub sync: &'a dyn VkSync,
}

impl std::fmt::Debug for VkPicture<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VkPicture")
            .field("image", &self.image)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("pool", &self.pool)
            .field("colour", &self.colour)
            .finish_non_exhaustive()
    }
}

/// How a submission that reads a [`VkPicture`] keeps step with the
/// decoder: it waits for the image's timeline semaphore at `wait` and
/// signals it at `wait + 1`.
pub trait VkSync {
    /// Locks the picture for one submission; [`VkSync::end`] right after
    /// the submission unlocks it.
    fn begin(&self) -> VkAccess;
    /// The submission is queued (or failed: `submitted` false): record
    /// the layout it left the image in.
    fn end(&self, submitted: bool, layout: i32, access: u64);
}

/// The state of a [`VkPicture`]'s image when a submission begins.
#[derive(Clone, Copy, Debug)]
pub struct VkAccess {
    /// `VkImageLayout`.
    pub layout: i32,
    /// `VkAccessFlags` of the last use.
    pub access: u64,
    /// The image's timeline semaphore and the value to wait for.
    pub semaphore: u64,
    pub wait: u64,
}

pub trait Decoder: Send {
    fn decode(&mut self, data: &[u8], out: &mut DecodedFrame) -> Result<(), CodecError>;

    /// The picture of the last decoded frame. `None` for codecs without
    /// real pictures (synthetic). Valid until the next `decode`.
    fn picture(&mut self, _prefer: PictureKind) -> Result<Option<Picture<'_>>, CodecError> {
        Ok(None)
    }
}

/// Which form of [`Picture`] the presenter would like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PictureKind {
    /// GPU memory, no trip through the CPU (falls back to it where
    /// impossible): [`Picture::DmaBuf`] or [`Picture::Exported`].
    Gpu,
    /// CPU memory (a download from the GPU).
    Nv12,
}
