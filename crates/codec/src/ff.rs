//! FFmpeg helpers shared by the hardware backends.

use std::ffi::{CStr, c_int};
use std::ptr;

use fernsicht_core::colour::{Colour, Depth, Primaries, Transfer};
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

/// Bytes a sample of a 4:2:0 frame in `format` takes: 2 for P010, else 1.
fn sample_bytes(format: c_int) -> usize {
    if format == ffi::AVPixelFormat::AV_PIX_FMT_P010LE as c_int {
        2
    } else {
        1
    }
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

/// Writes an NV12 or P010 CPU frame to `out`, tightly packed.
///
/// # Safety
/// `frame` is a valid NV12 or P010 frame with data.
pub(crate) unsafe fn nv12_from_frame(frame: *const ffi::AVFrame, out: &mut Vec<u8>) {
    // SAFETY: per the contract.
    unsafe {
        let f = &*frame;
        let (w, h) = (f.width as usize * sample_bytes(f.format), f.height as usize);
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

/// FFmpeg's pixel format of a 4:2:0 picture at `depth`: NV12 or P010.
pub(crate) fn yuv_format(depth: Depth) -> ffi::AVPixelFormat {
    match depth {
        Depth::Eight => ffi::AVPixelFormat::AV_PIX_FMT_NV12,
        Depth::Ten => ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
    }
}

/// FFmpeg's tags for `colour`: matrix, primaries, transfer.
pub(crate) fn tags(
    colour: Colour,
) -> (
    ffi::AVColorSpace,
    ffi::AVColorPrimaries,
    ffi::AVColorTransferCharacteristic,
) {
    use ffi::AVColorTransferCharacteristic as Trc;
    let (space, primaries) = match colour.primaries {
        Primaries::Bt709 => (
            ffi::AVColorSpace::AVCOL_SPC_BT709,
            ffi::AVColorPrimaries::AVCOL_PRI_BT709,
        ),
        Primaries::Bt2020 => (
            ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL,
            ffi::AVColorPrimaries::AVCOL_PRI_BT2020,
        ),
    };
    let trc = match colour.transfer {
        Transfer::Sdr => Trc::AVCOL_TRC_BT709,
        Transfer::Pq => Trc::AVCOL_TRC_SMPTE2084,
    };
    (space, primaries, trc)
}

/// Writes `colour` in limited range into the stream (VUI or sequence
/// header), so decoders convert back to RGB the way the encoder's input
/// was produced, and the chroma's siting: left (on the even columns), as
/// both converters produce it.
///
/// # Safety
/// `ctx` is a valid, not yet opened encoder context.
pub(crate) unsafe fn signal_colour(ctx: *mut ffi::AVCodecContext, colour: Colour) {
    let (space, primaries, trc) = tags(colour);
    // SAFETY: per the contract.
    unsafe {
        (*ctx).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        (*ctx).colorspace = space;
        (*ctx).color_primaries = primaries;
        (*ctx).color_trc = trc;
        (*ctx).chroma_sample_location = ffi::AVChromaLocation::AVCHROMA_LOC_LEFT;
    }
}

/// The software format of a hardware frame's pool (NV12, P010), or the
/// frame's own format for a CPU frame.
///
/// # Safety
/// `frame` is a valid AVFrame.
pub(crate) unsafe fn sw_format(frame: *const ffi::AVFrame) -> ffi::AVPixelFormat {
    // SAFETY: per the contract; hw_frames_ctx holds an AVHWFramesContext.
    unsafe {
        let f = &*frame;
        if f.hw_frames_ctx.is_null() {
            return if f.format == ffi::AVPixelFormat::AV_PIX_FMT_P010LE as c_int {
                ffi::AVPixelFormat::AV_PIX_FMT_P010LE
            } else {
                ffi::AVPixelFormat::AV_PIX_FMT_NV12
            };
        }
        (*((*f.hw_frames_ctx).data as *const ffi::AVHWFramesContext)).sw_format
    }
}

/// How a decoded frame is to be read: its depth (from its software format,
/// `sw`) and the colour its stream says (anything not BT.2020 or PQ reads
/// as BT.709 SDR, as before D4).
///
/// # Safety
/// `frame` is a valid AVFrame.
pub(crate) unsafe fn colour_of(frame: *const ffi::AVFrame, sw: ffi::AVPixelFormat) -> Colour {
    // SAFETY: per the contract.
    let f = unsafe { &*frame };
    let depth = if matches!(
        sw,
        ffi::AVPixelFormat::AV_PIX_FMT_P010LE | ffi::AVPixelFormat::AV_PIX_FMT_P016LE
    ) {
        Depth::Ten
    } else {
        Depth::Eight
    };
    let primaries = if f.color_primaries == ffi::AVColorPrimaries::AVCOL_PRI_BT2020 {
        Primaries::Bt2020
    } else {
        Primaries::Bt709
    };
    let transfer = if f.color_trc == ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084 {
        Transfer::Pq
    } else {
        Transfer::Sdr
    };
    Colour {
        depth,
        primaries,
        transfer,
    }
}

/// Constant bitrate at `kbps` with a buffer of one frame: the rate control
/// with the least delay.
///
/// # Safety
/// `ctx` is a valid encoder context.
pub(crate) unsafe fn set_cbr(ctx: *mut ffi::AVCodecContext, kbps: u32, fps: u32) {
    let bitrate = i64::from(kbps) * 1000;
    // SAFETY: per the contract.
    unsafe {
        (*ctx).bit_rate = bitrate;
        (*ctx).rc_max_rate = bitrate;
        (*ctx).rc_buffer_size = (bitrate / i64::from(fps.max(1))) as c_int;
    }
}

/// Opens FFmpeg's encoder `name`: a new context that `configure` fills,
/// opened with `opts`. Nothing is left behind when it fails.
///
/// # Safety
/// `configure` gets a valid, not yet opened context.
pub(crate) unsafe fn open_encoder(
    name: &CStr,
    opts: &[(&CStr, &CStr)],
    configure: impl FnOnce(*mut ffi::AVCodecContext),
) -> Result<*mut ffi::AVCodecContext, CodecError> {
    // SAFETY: the context is checked before use and freed on every error.
    unsafe {
        let codec = ffi::avcodec_find_encoder_by_name(name.as_ptr());
        if codec.is_null() {
            return Err(CodecError::Backend(format!(
                "FFmpeg has no {} encoder",
                name.to_string_lossy()
            )));
        }
        let mut ctx = ffi::avcodec_alloc_context3(codec);
        if ctx.is_null() {
            return Err(CodecError::Backend("out of memory".into()));
        }
        configure(ctx);
        let mut dict: *mut ffi::AVDictionary = ptr::null_mut();
        for (k, v) in opts {
            ffi::av_dict_set(&mut dict, k.as_ptr(), v.as_ptr(), 0);
        }
        let r = ffi::avcodec_open2(ctx, codec, &mut dict);
        ffi::av_dict_free(&mut dict);
        if let Err(e) = check(&format!("open {}", name.to_string_lossy()), r) {
            ffi::avcodec_free_context(&mut ctx);
            return Err(e);
        }
        Ok(ctx)
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
    /// The profile every hardware decoder takes: H.264 High, HEVC Main
    /// (Main 10 at 10 bit), AV1 Main (8 or 10 bit 4:2:0). `None` where the
    /// encoder has no such option (av1_nvenc: Main is all it makes).
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
    #[cfg_attr(not(feature = "vulkan"), allow(dead_code))]
    Vulkan,
}

pub(crate) fn names(
    codec: fernsicht_proto::Codec,
    backend: Backend,
    depth: Depth,
) -> Result<CodecNames, CodecError> {
    use fernsicht_proto::Codec;
    if depth == Depth::Ten && !matches!(codec, Codec::Hevc | Codec::Av1) {
        return Err(CodecError::Backend(format!(
            "{codec:?} in 10 bit: hardware decoders have no High 10"
        )));
    }
    Ok(match (codec, backend) {
        (Codec::H264, b) => CodecNames {
            decoder: c"h264",
            encoder: match b {
                Backend::Vaapi => c"h264_vaapi",
                Backend::Nvenc => c"h264_nvenc",
                Backend::Vulkan => c"h264_vulkan",
            },
            profile: Some(c"high"),
            label: "H.264",
        },
        (Codec::Hevc, b) => CodecNames {
            decoder: c"hevc",
            encoder: match b {
                Backend::Vaapi => c"hevc_vaapi",
                Backend::Nvenc => c"hevc_nvenc",
                Backend::Vulkan => c"hevc_vulkan",
            },
            profile: Some(match depth {
                Depth::Eight => c"main",
                Depth::Ten => c"main10",
            }),
            label: "HEVC",
        },
        (Codec::Av1, b) => CodecNames {
            decoder: c"av1",
            encoder: match b {
                Backend::Vaapi => c"av1_vaapi",
                Backend::Nvenc => c"av1_nvenc",
                Backend::Vulkan => c"av1_vulkan",
            },
            profile: match b {
                Backend::Vaapi => Some(c"main"),
                Backend::Nvenc | Backend::Vulkan => None,
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
