//! FFmpeg helpers shared by the hardware backends.

use std::ffi::{CStr, c_int};
use std::ptr;

use ffmpeg_next::ffi;

use crate::CodecError;

pub(crate) fn ff_err(what: &str, code: c_int) -> CodecError {
    let mut buf = [0 as std::ffi::c_char; 256];
    // SAFETY: av_strerror writes a NUL-terminated string into buf.
    let msg = unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    };
    CodecError::Backend(format!("{what}: {msg} ({code})"))
}

pub(crate) fn check(what: &str, code: c_int) -> Result<c_int, CodecError> {
    if code < 0 {
        Err(ff_err(what, code))
    } else {
        Ok(code)
    }
}

pub(crate) fn eagain(code: c_int) -> bool {
    code == -libc::EAGAIN
}

/// Copies tightly packed NV12 (`w`×`h`) into an allocated NV12 frame.
///
/// # Safety
/// `frame` is a writable NV12 frame of at least `w`×`h`; `nv12` holds
/// `w * h * 3 / 2` bytes.
pub(crate) unsafe fn nv12_into_frame(nv12: &[u8], frame: *mut ffi::AVFrame, w: usize, h: usize) {
    // SAFETY: per the contract.
    unsafe {
        let f = &*frame;
        let (y, uv) = nv12.split_at(w * h);
        for row in 0..h {
            let dst = f.data[0].add(row * f.linesize[0] as usize);
            ptr::copy_nonoverlapping(y.as_ptr().add(row * w), dst, w);
        }
        for row in 0..h / 2 {
            let dst = f.data[1].add(row * f.linesize[1] as usize);
            ptr::copy_nonoverlapping(uv.as_ptr().add(row * w), dst, w);
        }
    }
}

/// Appends an NV12 CPU frame to `out` as tightly packed NV12.
///
/// # Safety
/// `frame` is a valid NV12 frame with data.
pub(crate) unsafe fn nv12_from_frame(frame: *const ffi::AVFrame, out: &mut Vec<u8>) {
    // SAFETY: per the contract.
    unsafe {
        let f = &*frame;
        let (w, h) = (f.width as usize, f.height as usize);
        out.clear();
        out.reserve(w * h * 3 / 2);
        for row in 0..h {
            let src = f.data[0].add(row * f.linesize[0] as usize);
            out.extend_from_slice(std::slice::from_raw_parts(src, w));
        }
        for row in 0..h / 2 {
            let src = f.data[1].add(row * f.linesize[1] as usize);
            out.extend_from_slice(std::slice::from_raw_parts(src, w));
        }
    }
}

/// Writes BT.709 limited range into the stream (VUI), so decoders convert
/// back to RGB the way the encoder's input was produced.
///
/// # Safety
/// `ctx` is a valid, not yet opened encoder context.
pub(crate) unsafe fn signal_bt709_limited(ctx: *mut ffi::AVCodecContext) {
    // SAFETY: per the contract.
    unsafe {
        (*ctx).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        (*ctx).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
        (*ctx).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
        (*ctx).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
    }
}

/// Whether a decoded frame says BT.709 limited range.
///
/// # Safety
/// `frame` is a valid AVFrame.
pub(crate) unsafe fn is_bt709_limited(frame: *const ffi::AVFrame) -> bool {
    // SAFETY: per the contract.
    let f = unsafe { &*frame };
    f.color_range == ffi::AVColorRange::AVCOL_RANGE_MPEG
        && f.colorspace == ffi::AVColorSpace::AVCOL_SPC_BT709
}

/// FFmpeg's names for a codec on one hardware backend.
pub(crate) struct CodecNames {
    /// FFmpeg's own decoder, which uses the hardware through the device
    /// context. Chosen by name: for AV1, FFmpeg lists the CPU decoder
    /// libdav1d first.
    pub decoder: &'static CStr,
    pub encoder: &'static CStr,
    /// The profile every hardware decoder takes: H.264 High, HEVC Main,
    /// AV1 Main (8 bit 4:2:0). `None` where the encoder has no such option
    /// (av1_nvenc: Main is all it makes).
    pub profile: Option<&'static CStr>,
    pub label: &'static str,
}

/// Hardware encoder backends, as FFmpeg suffixes their encoders.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Backend {
    #[cfg_attr(not(feature = "vaapi"), allow(dead_code))]
    Vaapi,
    #[cfg_attr(not(feature = "nvidia"), allow(dead_code))]
    Nvenc,
}

pub(crate) fn names(
    codec: fernsicht_proto::Codec,
    backend: Backend,
) -> Result<CodecNames, CodecError> {
    use fernsicht_proto::Codec;
    Ok(match (codec, backend) {
        (Codec::H264, b) => CodecNames {
            decoder: c"h264",
            encoder: match b {
                Backend::Vaapi => c"h264_vaapi",
                Backend::Nvenc => c"h264_nvenc",
            },
            profile: Some(c"high"),
            label: "H.264",
        },
        (Codec::Hevc, b) => CodecNames {
            decoder: c"hevc",
            encoder: match b {
                Backend::Vaapi => c"hevc_vaapi",
                Backend::Nvenc => c"hevc_nvenc",
            },
            profile: Some(c"main"),
            label: "HEVC",
        },
        (Codec::Av1, b) => CodecNames {
            decoder: c"av1",
            encoder: match b {
                Backend::Vaapi => c"av1_vaapi",
                Backend::Nvenc => c"av1_nvenc",
            },
            profile: match b {
                Backend::Vaapi => Some(c"main"),
                Backend::Nvenc => None,
            },
            label: "AV1",
        },
        (other, _) => {
            return Err(CodecError::Backend(format!(
                "{other:?} is not supported by the hardware backends"
            )));
        }
    })
}
