//! Video encode and decode.
//!
//! Backends:
//! - [`synthetic`]: produces a checksummed payload of realistic size for the
//!   configured bitrate. No real compression; it lets transport, FEC and
//!   latency measurement run on machines without a GPU.
//! - VAAPI (planned, phase 1): H.264 low-latency with DMA-BUF import
//!   (radeonsi on the RX 7800 XT, iHD on Intel), decode into a Vulkan
//!   texture on the client. NVENC for NVIDIA.

pub mod synthetic;

use fernsicht_capture::Frame;
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
}

/// Decoder output: for now metadata only; GPU backends will hand out a
/// texture handle here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub seq: u64,
    pub keyframe: bool,
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("corrupt bitstream: {0}")]
    Corrupt(&'static str),
    #[error("waiting for a keyframe")]
    NeedKeyframe,
    #[error("codec backend: {0}")]
    Backend(String),
}

pub trait Encoder: Send {
    fn codec(&self) -> Codec;

    fn encode(&mut self, frame: &Frame, out: &mut EncodedFrame) -> Result<(), CodecError>;

    /// The next frame will be a keyframe (or start an intra refresh).
    fn request_keyframe(&mut self);

    fn set_bitrate(&mut self, kbps: u32);
}

pub trait Decoder: Send {
    fn decode(&mut self, data: &[u8], out: &mut DecodedFrame) -> Result<(), CodecError>;
}
