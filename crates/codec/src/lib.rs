//! Video encode and decode.
//!
//! Backends:
//! - [`synthetic`]: produces a checksummed payload of realistic size for the
//!   configured bitrate. No real compression; it lets transport, FEC and
//!   latency measurement run on machines without a GPU.
//! - [`vaapi`] (feature `vaapi`): H.264 low-latency encode and decode in
//!   hardware via FFmpeg (radeonsi on the RX 7800 XT, iHD on Intel).
//!   Frames are uploaded from CPU memory for now; DMA-BUF import from KMS
//!   capture comes with the capture backend. NVENC for NVIDIA follows.

pub mod synthetic;
#[cfg(feature = "vaapi")]
pub mod vaapi;

use fernsicht_capture::{DmaBuf, Frame};
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

/// The picture of the last decoded frame, for the presenter.
#[derive(Debug)]
pub enum Picture<'a> {
    /// Tightly packed NV12 in CPU memory (BT.709, limited range).
    Nv12 {
        width: u32,
        height: u32,
        data: &'a [u8],
    },
    /// NV12 on the GPU (fourcc `NV12`, plane 0 Y, plane 1 interleaved UV).
    /// `key` stays the same for the same underlying surface, so presenters
    /// can keep the import instead of repeating it every frame.
    DmaBuf { image: &'a DmaBuf, key: u64 },
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
    /// GPU memory, no copy (falls back to CPU where impossible).
    DmaBuf,
    /// CPU memory (a download from the GPU).
    Nv12,
}
