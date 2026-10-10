//! H.264 encode and decode in hardware over VAAPI, via FFmpeg.
//!
//! Works with AMD (radeonsi) and Intel (iHD). Settings follow what Sunshine
//! uses for low latency:
//!
//! - no B-frames, so every input frame produces its packet immediately
//! - an endless GOP: keyframes only on request (stream start, loss)
//! - CBR with a buffer of one frame, so frame sizes stay even
//! - `async_depth = 1`: the encoder does not queue frames internally
//!
//! Input frames are NV12 in CPU memory and are uploaded per frame. Zero-copy
//! DMA-BUF import from KMS capture replaces the upload later.

use std::ffi::{CStr, CString, c_int};
use std::ptr;

use fernsicht_capture::{Frame, PixelFormat};
use fernsicht_core::now_us;
use fernsicht_proto::Codec;
use ffmpeg_next::ffi;

use crate::{CodecError, DecodedFrame, Decoder, EncodedFrame, Encoder};

/// Render node used when none is given.
pub const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";

fn ff_err(what: &str, code: c_int) -> CodecError {
    let mut buf = [0 as std::ffi::c_char; 256];
    // SAFETY: av_strerror writes a NUL-terminated string into buf.
    let msg = unsafe {
        ffi::av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    };
    CodecError::Backend(format!("{what}: {msg} ({code})"))
}

fn check(what: &str, code: c_int) -> Result<c_int, CodecError> {
    if code < 0 {
        Err(ff_err(what, code))
    } else {
        Ok(code)
    }
}

fn eagain(code: c_int) -> bool {
    code == -libc::EAGAIN
}

#[link(name = "va")]
unsafe extern "C" {
    /// Blocks until all work on `surface` (here: decoding into it) is done.
    fn vaSyncSurface(dpy: ffi::VADisplay, surface: ffi::VASurfaceID) -> c_int;
}

/// A VAAPI device (one render node), shared by encoder and decoder contexts.
pub struct VaapiDevice {
    ctx: *mut ffi::AVBufferRef,
}

// SAFETY: the device reference is only used by the thread that owns the
// encoder or decoder holding it; FFmpeg device contexts are thread-safe to
// reference-count.
unsafe impl Send for VaapiDevice {}

impl VaapiDevice {
    pub fn open(render_node: &str) -> Result<Self, CodecError> {
        let node = CString::new(render_node)
            .map_err(|_| CodecError::Backend("render node path contains NUL".into()))?;
        let mut ctx = ptr::null_mut();
        // SAFETY: valid out-pointer and NUL-terminated device string.
        check("open VAAPI device", unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut ctx,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                node.as_ptr(),
                ptr::null_mut(),
                0,
            )
        })?;
        Ok(Self { ctx })
    }

    fn display(&self) -> ffi::VADisplay {
        // SAFETY: a VAAPI device context's hwctx is an AVVAAPIDeviceContext.
        unsafe {
            let dc = (*self.ctx).data as *mut ffi::AVHWDeviceContext;
            (*((*dc).hwctx as *mut ffi::AVVAAPIDeviceContext)).display
        }
    }

    fn new_ref(&self) -> *mut ffi::AVBufferRef {
        // SAFETY: self.ctx is a valid buffer reference.
        unsafe { ffi::av_buffer_ref(self.ctx) }
    }
}

impl Drop for VaapiDevice {
    fn drop(&mut self) {
        // SAFETY: we own this reference.
        unsafe { ffi::av_buffer_unref(&mut self.ctx) };
    }
}

/// Encoder settings.
#[derive(Clone, Debug)]
pub struct VaapiEncoderConfig {
    pub render_node: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

pub struct VaapiEncoder {
    _device: VaapiDevice,
    ctx: *mut ffi::AVCodecContext,
    frames: *mut ffi::AVBufferRef,
    sw: *mut ffi::AVFrame,
    hw: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    width: u32,
    height: u32,
    force_keyframe: bool,
    next_pts: i64,
}

// SAFETY: all FFmpeg objects are owned exclusively by this value and only
// touched through &mut self.
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    pub fn new(cfg: &VaapiEncoderConfig) -> Result<Self, CodecError> {
        if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) || cfg.fps == 0 {
            return Err(CodecError::Backend(
                "width/height must be even, fps > 0".into(),
            ));
        }
        let device = VaapiDevice::open(&cfg.render_node)?;
        // SAFETY: every pointer below is checked before use and released in
        // Drop; the struct is built incrementally so Drop sees nulls for
        // anything not yet created.
        unsafe {
            let codec = ffi::avcodec_find_encoder_by_name(c"h264_vaapi".as_ptr());
            if codec.is_null() {
                return Err(CodecError::Backend(
                    "FFmpeg has no h264_vaapi encoder".into(),
                ));
            }
            let mut enc = Self {
                _device: device,
                ctx: ffi::avcodec_alloc_context3(codec),
                frames: ptr::null_mut(),
                sw: ffi::av_frame_alloc(),
                hw: ffi::av_frame_alloc(),
                pkt: ffi::av_packet_alloc(),
                width: cfg.width,
                height: cfg.height,
                force_keyframe: true,
                next_pts: 0,
            };
            if enc.ctx.is_null() || enc.sw.is_null() || enc.hw.is_null() || enc.pkt.is_null() {
                return Err(CodecError::Backend("out of memory".into()));
            }

            // Hardware frame pool the uploads go into.
            enc.frames = ffi::av_hwframe_ctx_alloc(enc._device.ctx);
            if enc.frames.is_null() {
                return Err(CodecError::Backend("av_hwframe_ctx_alloc failed".into()));
            }
            let fc = (*enc.frames).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            (*fc).width = cfg.width as c_int;
            (*fc).height = cfg.height as c_int;
            (*fc).initial_pool_size = 4;
            check(
                "init VAAPI frame pool",
                ffi::av_hwframe_ctx_init(enc.frames),
            )?;

            let c = enc.ctx;
            let fps = cfg.fps as c_int;
            let bitrate = i64::from(cfg.bitrate_kbps) * 1000;
            (*c).width = cfg.width as c_int;
            (*c).height = cfg.height as c_int;
            (*c).time_base = ffi::AVRational { num: 1, den: fps };
            (*c).framerate = ffi::AVRational { num: fps, den: 1 };
            (*c).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*c).hw_frames_ctx = ffi::av_buffer_ref(enc.frames);
            (*c).max_b_frames = 0;
            (*c).gop_size = c_int::MAX;
            (*c).keyint_min = c_int::MAX;
            (*c).bit_rate = bitrate;
            (*c).rc_max_rate = bitrate;
            (*c).rc_buffer_size = (bitrate / i64::from(fps)) as c_int;
            (*c).flags |= (ffi::AV_CODEC_FLAG_LOW_DELAY | ffi::AV_CODEC_FLAG_CLOSED_GOP) as c_int;
            (*c).thread_count = 1;

            let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
            for (k, v) in [
                (c"rc_mode", c"CBR"),
                (c"async_depth", c"1"),
                (c"profile", c"high"),
            ] {
                ffi::av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
            }
            let r = ffi::avcodec_open2(c, codec, &mut opts);
            ffi::av_dict_free(&mut opts);
            check("open h264_vaapi", r)?;

            (*enc.sw).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            (*enc.sw).width = cfg.width as c_int;
            (*enc.sw).height = cfg.height as c_int;
            check("allocate upload frame", ffi::av_frame_get_buffer(enc.sw, 0))?;
            Ok(enc)
        }
    }

    /// Copies an NV12 frame into the reusable CPU frame.
    unsafe fn fill_sw(&mut self, frame: &Frame) -> Result<(), CodecError> {
        let (w, h) = (self.width as usize, self.height as usize);
        // SAFETY (whole fn): sw was allocated for w×h NV12 in new(); the
        // source slices are bounds-checked by the caller.
        unsafe {
            check(
                "make upload frame writable",
                ffi::av_frame_make_writable(self.sw),
            )?;
            let sw = &*self.sw;
            let (y, uv) = frame.data.split_at(w * h);
            for row in 0..h {
                let dst = sw.data[0].add(row * sw.linesize[0] as usize);
                ptr::copy_nonoverlapping(y.as_ptr().add(row * w), dst, w);
            }
            for row in 0..h / 2 {
                let dst = sw.data[1].add(row * sw.linesize[1] as usize);
                ptr::copy_nonoverlapping(uv.as_ptr().add(row * w), dst, w);
            }
        }
        Ok(())
    }
}

impl Encoder for VaapiEncoder {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn encode(&mut self, frame: &Frame, out: &mut EncodedFrame) -> Result<(), CodecError> {
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
            self.fill_sw(frame)?;
            ffi::av_frame_unref(self.hw);
            check(
                "get VAAPI surface",
                ffi::av_hwframe_get_buffer(self.frames, self.hw, 0),
            )?;
            check(
                "upload to VAAPI surface",
                ffi::av_hwframe_transfer_data(self.hw, self.sw, 0),
            )?;
            (*self.hw).pts = self.next_pts;
            self.next_pts += 1;
            if self.force_keyframe {
                // Makes the VAAPI encoder emit an IDR frame.
                (*self.hw).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
            }
            check("send frame", ffi::avcodec_send_frame(self.ctx, self.hw))?;
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
            // With no B-frames and async_depth 1 this must not happen; a
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
        // Changing the rate control of an open VAAPI encoder needs a
        // re-init; adaptive bitrate comes with congestion control (phase 3).
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        // SAFETY: each pointer is either null or owned by us.
        unsafe {
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_frame_free(&mut self.sw);
            ffi::av_frame_free(&mut self.hw);
            ffi::av_packet_free(&mut self.pkt);
            ffi::av_buffer_unref(&mut self.frames);
        }
    }
}

/// Picks VAAPI surfaces as decoder output.
unsafe extern "C" fn pick_vaapi(
    _ctx: *mut ffi::AVCodecContext,
    mut fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    // SAFETY: FFmpeg passes a list terminated by AV_PIX_FMT_NONE.
    unsafe {
        while *fmts != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *fmts == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *fmts;
            }
            fmts = fmts.add(1);
        }
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

pub struct VaapiDecoder {
    _device: VaapiDevice,
    ctx: *mut ffi::AVCodecContext,
    /// The last decoded surface.
    frame: *mut ffi::AVFrame,
    /// Receives from the decoder; moved into `frame` on success, because
    /// avcodec_receive_frame clears its target even when it has nothing.
    recv: *mut ffi::AVFrame,
    sw: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    decoded: u64,
}

// SAFETY: as for VaapiEncoder.
unsafe impl Send for VaapiDecoder {}

impl VaapiDecoder {
    pub fn new(render_node: &str) -> Result<Self, CodecError> {
        let device = VaapiDevice::open(render_node)?;
        // SAFETY: see VaapiEncoder::new.
        unsafe {
            let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264);
            if codec.is_null() {
                return Err(CodecError::Backend("FFmpeg has no H.264 decoder".into()));
            }
            let dec = Self {
                ctx: ffi::avcodec_alloc_context3(codec),
                frame: ffi::av_frame_alloc(),
                recv: ffi::av_frame_alloc(),
                sw: ffi::av_frame_alloc(),
                pkt: ffi::av_packet_alloc(),
                decoded: 0,
                _device: device,
            };
            if dec.ctx.is_null()
                || dec.frame.is_null()
                || dec.recv.is_null()
                || dec.sw.is_null()
                || dec.pkt.is_null()
            {
                return Err(CodecError::Backend("out of memory".into()));
            }
            (*dec.ctx).hw_device_ctx = dec._device.new_ref();
            (*dec.ctx).get_format = Some(pick_vaapi);
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

    /// Downloads the last decoded frame as tightly packed NV12 (tests and
    /// diagnostics; the client presents the VAAPI surface directly).
    pub fn last_frame_nv12(&mut self) -> Result<Vec<u8>, CodecError> {
        // SAFETY: frame holds the last decoded VAAPI surface (or nothing,
        // in which case the transfer fails cleanly).
        unsafe {
            ffi::av_frame_unref(self.sw);
            (*self.sw).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            check(
                "download surface",
                ffi::av_hwframe_transfer_data(self.sw, self.frame, 0),
            )?;
            let sw = &*self.sw;
            let (w, h) = (sw.width as usize, sw.height as usize);
            let mut out = Vec::with_capacity(w * h * 3 / 2);
            for row in 0..h {
                let src = sw.data[0].add(row * sw.linesize[0] as usize);
                out.extend_from_slice(std::slice::from_raw_parts(src, w));
            }
            for row in 0..h / 2 {
                let src = sw.data[1].add(row * sw.linesize[1] as usize);
                out.extend_from_slice(std::slice::from_raw_parts(src, w));
            }
            Ok(out)
        }
    }
}

impl Decoder for VaapiDecoder {
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
            // avcodec_receive_frame returns once decoding is *queued* on the
            // GPU. Wait for it to finish: the presenter needs the picture,
            // and the Decode stage in the latency overlay must be honest.
            let surface = (*self.frame).data[3] as usize as ffi::VASurfaceID;
            let r = vaSyncSurface(self._device.display(), surface);
            if r != 0 {
                return Err(CodecError::Backend(format!("vaSyncSurface failed: {r}")));
            }
        }
        Ok(())
    }
}

impl Drop for VaapiDecoder {
    fn drop(&mut self) {
        // SAFETY: each pointer is either null or owned by us.
        unsafe {
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_frame_free(&mut self.frame);
            ffi::av_frame_free(&mut self.recv);
            ffi::av_frame_free(&mut self.sw);
            ffi::av_packet_free(&mut self.pkt);
        }
    }
}
