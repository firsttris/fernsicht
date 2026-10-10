//! H.264 encode and decode on NVIDIA GPUs (NVENC, NVDEC), via FFmpeg.
//!
//! The encoder uses NVIDIA's low-latency settings, the counterpart of the
//! VAAPI ones: preset p1, tune "ull", zero latency, no B-frames, no
//! lookahead, `delay = 0` (a packet per input frame, synchronously), CBR
//! with a buffer of one frame, an endless GOP with forced IDR frames on
//! request.
//!
//! Input and output are NV12 in CPU memory for now (FFmpeg uploads and
//! downloads, about 0.5 ms each way at 1080p). Capture on NVIDIA (DMA-BUF
//! into CUDA) and showing decoded frames without a copy come later.
//!
//! CUDA is loaded at runtime by FFmpeg, so this builds anywhere; without
//! the NVIDIA driver creating an encoder or decoder fails cleanly.

use std::ffi::{CString, c_int};
use std::ptr;

use fernsicht_capture::{Frame, PixelFormat};
use fernsicht_core::now_us;
use fernsicht_proto::Codec;
use ffmpeg_next::ffi;

use crate::ff::{
    check, eagain, ff_err, is_bt709_limited, nv12_from_frame, nv12_into_frame, signal_bt709_limited,
};
use crate::{CodecError, DecodedFrame, Decoder, EncodedFrame, Encoder, Picture, PictureKind};

/// Encoder settings.
#[derive(Clone, Debug)]
pub struct NvencConfig {
    /// CUDA device index (0 = first NVIDIA GPU).
    pub gpu: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

pub struct NvencEncoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    width: u32,
    height: u32,
    force_keyframe: bool,
    next_pts: i64,
}

// SAFETY: all FFmpeg objects are owned exclusively by this value and only
// touched through &mut self.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    pub fn new(cfg: &NvencConfig) -> Result<Self, CodecError> {
        if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) || cfg.fps == 0 {
            return Err(CodecError::Backend(
                "width/height must be even, fps > 0".into(),
            ));
        }
        // SAFETY: every pointer is checked before use and released in Drop,
        // which also handles a partly built value.
        unsafe {
            let codec = ffi::avcodec_find_encoder_by_name(c"h264_nvenc".as_ptr());
            if codec.is_null() {
                return Err(CodecError::Backend(
                    "FFmpeg has no h264_nvenc encoder".into(),
                ));
            }
            let enc = Self {
                ctx: ffi::avcodec_alloc_context3(codec),
                frame: ffi::av_frame_alloc(),
                pkt: ffi::av_packet_alloc(),
                width: cfg.width,
                height: cfg.height,
                force_keyframe: true,
                next_pts: 0,
            };
            if enc.ctx.is_null() || enc.frame.is_null() || enc.pkt.is_null() {
                return Err(CodecError::Backend("out of memory".into()));
            }
            let c = enc.ctx;
            let fps = cfg.fps as c_int;
            let bitrate = i64::from(cfg.bitrate_kbps) * 1000;
            (*c).width = cfg.width as c_int;
            (*c).height = cfg.height as c_int;
            (*c).time_base = ffi::AVRational { num: 1, den: fps };
            (*c).framerate = ffi::AVRational { num: fps, den: 1 };
            (*c).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            (*c).max_b_frames = 0;
            (*c).gop_size = c_int::MAX;
            (*c).bit_rate = bitrate;
            (*c).rc_max_rate = bitrate;
            (*c).rc_buffer_size = (bitrate / i64::from(fps)) as c_int;
            (*c).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as c_int;
            signal_bt709_limited(c);

            let gpu = CString::new(cfg.gpu.to_string()).expect("digits");
            let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
            for (k, v) in [
                (c"preset", c"p1"),
                (c"tune", c"ull"),
                (c"rc", c"cbr"),
                (c"zerolatency", c"1"),
                (c"delay", c"0"),
                (c"rc-lookahead", c"0"),
                (c"forced-idr", c"1"),
                (c"profile", c"high"),
            ] {
                ffi::av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
            }
            ffi::av_dict_set(&mut opts, c"gpu".as_ptr(), gpu.as_ptr(), 0);
            let r = ffi::avcodec_open2(c, codec, &mut opts);
            ffi::av_dict_free(&mut opts);
            check("open h264_nvenc", r)?;

            (*enc.frame).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            (*enc.frame).width = cfg.width as c_int;
            (*enc.frame).height = cfg.height as c_int;
            check(
                "allocate input frame",
                ffi::av_frame_get_buffer(enc.frame, 0),
            )?;
            Ok(enc)
        }
    }
}

impl Encoder for NvencEncoder {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn encode(&mut self, frame: &Frame, out: &mut EncodedFrame) -> Result<(), CodecError> {
        if frame.dmabuf.is_some() {
            return Err(CodecError::Backend(
                "NVENC takes CPU frames only for now; screen capture needs the VAAPI encoder"
                    .into(),
            ));
        }
        if frame.format != PixelFormat::Nv12
            || frame.width != self.width
            || frame.height != self.height
            || frame.data.len() < PixelFormat::Nv12.frame_bytes(self.width, self.height)
        {
            return Err(CodecError::Backend(
                "input must be NV12 at the configured size".into(),
            ));
        }
        out.data.clear();
        out.keyframe = false;
        // SAFETY: all pointers were created in new() and stay valid.
        unsafe {
            check(
                "make input frame writable",
                ffi::av_frame_make_writable(self.frame),
            )?;
            nv12_into_frame(
                &frame.data,
                self.frame,
                self.width as usize,
                self.height as usize,
            );
            let f = &mut *self.frame;
            f.pts = self.next_pts;
            self.next_pts += 1;
            f.pict_type = if self.force_keyframe {
                // With forced-idr this is an IDR frame.
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            check("send frame", ffi::avcodec_send_frame(self.ctx, self.frame))?;
            self.force_keyframe = false;
            loop {
                let r = ffi::avcodec_receive_packet(self.ctx, self.pkt);
                if eagain(r) {
                    break;
                }
                check("receive packet", r)?;
                let p = &*self.pkt;
                out.data
                    .extend_from_slice(std::slice::from_raw_parts(p.data, p.size as usize));
                out.keyframe |= p.flags & ffi::AV_PKT_FLAG_KEY as c_int != 0;
                ffi::av_packet_unref(self.pkt);
            }
        }
        if out.data.is_empty() {
            // delay = 0 makes NVENC return each packet right away; a
            // buffering encoder would add a frame of latency.
            return Err(CodecError::Backend(
                "encoder returned no packet for the frame".into(),
            ));
        }
        out.seq = frame.seq;
        out.capture_us = frame.capture_us;
        out.capture_ready_us = frame.ready_us;
        out.encoded_us = now_us();
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, _kbps: u32) {
        // Adaptive bitrate comes with congestion control (phase 3).
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        // SAFETY: each pointer is either null or owned by us.
        unsafe {
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_frame_free(&mut self.frame);
            ffi::av_packet_free(&mut self.pkt);
        }
    }
}

/// Picks CUDA frames (NVDEC) as decoder output.
unsafe extern "C" fn pick_cuda(
    _ctx: *mut ffi::AVCodecContext,
    mut fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    // SAFETY: FFmpeg passes a list terminated by AV_PIX_FMT_NONE.
    unsafe {
        while *fmts != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *fmts == ffi::AVPixelFormat::AV_PIX_FMT_CUDA {
                return *fmts;
            }
            fmts = fmts.add(1);
        }
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

pub struct NvdecDecoder {
    device: *mut ffi::AVBufferRef,
    ctx: *mut ffi::AVCodecContext,
    /// The last decoded frame (CUDA memory).
    frame: *mut ffi::AVFrame,
    /// Receives from the decoder; moved into `frame` on success.
    recv: *mut ffi::AVFrame,
    sw: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    decoded: u64,
    /// The last downloaded picture.
    nv12: Vec<u8>,
}

// SAFETY: as for NvencEncoder.
unsafe impl Send for NvdecDecoder {}

impl NvdecDecoder {
    /// `gpu`: CUDA device index.
    pub fn new(gpu: u32) -> Result<Self, CodecError> {
        let index = CString::new(gpu.to_string()).expect("digits");
        // SAFETY: as in NvencEncoder::new.
        unsafe {
            let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264);
            if codec.is_null() {
                return Err(CodecError::Backend("FFmpeg has no H.264 decoder".into()));
            }
            let mut dec = Self {
                device: ptr::null_mut(),
                ctx: ffi::avcodec_alloc_context3(codec),
                frame: ffi::av_frame_alloc(),
                recv: ffi::av_frame_alloc(),
                sw: ffi::av_frame_alloc(),
                pkt: ffi::av_packet_alloc(),
                decoded: 0,
                nv12: Vec::new(),
            };
            if dec.ctx.is_null()
                || dec.frame.is_null()
                || dec.recv.is_null()
                || dec.sw.is_null()
                || dec.pkt.is_null()
            {
                return Err(CodecError::Backend("out of memory".into()));
            }
            check("open CUDA device (NVIDIA driver?)", {
                ffi::av_hwdevice_ctx_create(
                    &mut dec.device,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                    index.as_ptr(),
                    ptr::null_mut(),
                    0,
                )
            })?;
            (*dec.ctx).hw_device_ctx = ffi::av_buffer_ref(dec.device);
            (*dec.ctx).get_format = Some(pick_cuda);
            (*dec.ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as c_int;
            // Frame threading would delay output by a frame per thread.
            (*dec.ctx).thread_count = 1;
            check(
                "open H.264 decoder",
                ffi::avcodec_open2(dec.ctx, codec, ptr::null_mut()),
            )?;
            Ok(dec)
        }
    }

    /// Whether the stream says its last frame is BT.709 limited range.
    pub fn last_frame_is_bt709_limited(&self) -> bool {
        // SAFETY: frame is a valid (possibly empty) AVFrame.
        unsafe { is_bt709_limited(self.frame) }
    }

    /// Downloads the last decoded frame as tightly packed NV12.
    pub fn last_frame_nv12(&mut self) -> Result<Vec<u8>, CodecError> {
        self.download()?;
        Ok(self.nv12.clone())
    }

    fn download(&mut self) -> Result<(), CodecError> {
        // SAFETY: frame holds the last decoded CUDA frame (or nothing, in
        // which case the transfer fails cleanly).
        unsafe {
            ffi::av_frame_unref(self.sw);
            (*self.sw).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            check(
                "download frame",
                ffi::av_hwframe_transfer_data(self.sw, self.frame, 0),
            )?;
            nv12_from_frame(self.sw, &mut self.nv12);
        }
        Ok(())
    }
}

impl Decoder for NvdecDecoder {
    fn decode(&mut self, data: &[u8], out: &mut DecodedFrame) -> Result<(), CodecError> {
        if data.is_empty() || data.len() > i32::MAX as usize {
            return Err(CodecError::Corrupt("empty or oversized packet"));
        }
        // SAFETY: pkt is ours; av_new_packet allocates len bytes plus padding.
        unsafe {
            ffi::av_packet_unref(self.pkt);
            check(
                "allocate packet",
                ffi::av_new_packet(self.pkt, data.len() as c_int),
            )?;
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let r = ffi::avcodec_send_packet(self.ctx, self.pkt);
            if r < 0 {
                return Err(if r == ffi::AVERROR_INVALIDDATA {
                    CodecError::Corrupt("invalid H.264 data")
                } else {
                    ff_err("send packet", r)
                });
            }
            let mut got = false;
            loop {
                let r = ffi::avcodec_receive_frame(self.ctx, self.recv);
                if eagain(r) {
                    break;
                }
                check("receive frame", r)?;
                ffi::av_frame_unref(self.frame);
                ffi::av_frame_move_ref(self.frame, self.recv);
                got = true;
                self.decoded += 1;
                let f = &*self.frame;
                *out = DecodedFrame {
                    width: f.width as u32,
                    height: f.height as u32,
                    seq: self.decoded - 1,
                    keyframe: f.flags & ffi::AV_FRAME_FLAG_KEY as c_int != 0,
                };
            }
            if !got {
                return Err(CodecError::Corrupt("decoder produced no frame"));
            }
        }
        Ok(())
    }

    /// Always NV12 in CPU memory for now, whatever is preferred.
    fn picture(&mut self, _prefer: PictureKind) -> Result<Option<Picture<'_>>, CodecError> {
        // SAFETY: frame is a valid AVFrame.
        let (format, width, height) = unsafe {
            let f = &*self.frame;
            (f.format, f.width as u32, f.height as u32)
        };
        if format != ffi::AVPixelFormat::AV_PIX_FMT_CUDA as c_int {
            return Ok(None);
        }
        self.download()?;
        Ok(Some(Picture::Nv12 {
            width,
            height,
            data: &self.nv12,
        }))
    }
}

impl Drop for NvdecDecoder {
    fn drop(&mut self) {
        // SAFETY: each pointer is either null or owned by us.
        unsafe {
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_frame_free(&mut self.frame);
            ffi::av_frame_free(&mut self.recv);
            ffi::av_frame_free(&mut self.sw);
            ffi::av_packet_free(&mut self.pkt);
            ffi::av_buffer_unref(&mut self.device);
        }
    }
}
