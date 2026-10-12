//! H.264, HEVC and AV1 encode and decode on NVIDIA GPUs (NVENC, NVDEC),
//! via FFmpeg (AV1 decoding from RTX 30, encoding from RTX 40).
//!
//! The encoder uses NVIDIA's low-latency settings, the counterpart of the
//! VAAPI ones: preset p1, tune "ull", zero latency, no B-frames, no
//! lookahead, `delay = 0` (a packet per input frame, synchronously), CBR
//! with a buffer of one frame, an endless GOP with forced IDR frames on
//! request.
//!
//! Encoder input is NV12 in CPU memory (uploaded by FFmpeg) or the screen
//! as an RGB DMA-BUF (KMS capture): Vulkan converts it to NV12 (BT.709,
//! scaled to the stream size) into memory CUDA imported, and CUDA copies it
//! into NVENC's input, so the picture never leaves the GPU. Decoder output
//! is downloaded to CPU memory for now.
//!
//! CUDA is loaded at runtime by FFmpeg, so this builds anywhere; without
//! the NVIDIA driver creating an encoder or decoder fails cleanly.

use std::ffi::{CString, c_int};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fernsicht_capture::{DmaBuf, Frame, PixelFormat};
use fernsicht_core::now_us;
use fernsicht_gpu::convert::Converter;
use fernsicht_gpu::{Gpu, Prefer};
use fernsicht_proto::Codec;
use ffmpeg_next::ffi;

use crate::cuda;
use crate::ff;
use crate::ff::{
    check, eagain, ff_err, is_bt709_limited, nv12_from_frame, nv12_into_frame, signal_bt709_limited,
};
use crate::{CodecError, DecodedFrame, Decoder, EncodedFrame, Encoder, Picture, PictureKind};

/// Encoder settings.
#[derive(Clone, Debug)]
pub struct NvencConfig {
    /// CUDA device index (0 = first NVIDIA GPU).
    pub gpu: u32,
    /// H.264, HEVC or AV1.
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Heal losses by intra refresh: every [`REFRESH_PERIOD_S`] the whole
    /// picture is refreshed column by column, so no keyframe is needed
    /// after a loss (the first frame and forced ones still are).
    pub intra_refresh: bool,
}

/// How long an intra refresh wave takes to cross the picture.
pub const REFRESH_PERIOD_S: f64 = 0.5;

pub struct NvencEncoder {
    device: *mut ffi::AVBufferRef,
    frames: *mut ffi::AVBufferRef,
    ctx: *mut ffi::AVCodecContext,
    /// CPU input, uploaded into `hw`.
    sw: *mut ffi::AVFrame,
    /// What NVENC encodes: NV12 in CUDA memory.
    hw: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    intra_refresh: bool,
    /// Vulkan conversion of DMA-BUF input, set up on the first such frame.
    gpu_input: Option<Result<GpuInput, String>>,
    force_keyframe: bool,
    /// A new bitrate set on the open context: FFmpeg reconfigures NVENC
    /// with the next frame, which is then an IDR frame.
    rate_change: Option<u32>,
    next_pts: i64,
}

/// Screen capture input: the DMA-BUF is converted to NV12 by Vulkan into a
/// buffer CUDA imported once, and NVENC reads that buffer as its input
/// frame (no copy).
struct GpuInput {
    /// The buffer as FFmpeg refers to a frame's memory: refcounted, never
    /// freed by FFmpeg (CUDA owns the mapping, Vulkan the memory).
    frame_buf: *mut ffi::AVBufferRef,
    // Drop order: CUDA lets go of the memory before Vulkan frees it.
    buffer: cuda::ImportedBuffer,
    converter: Converter,
}

impl Drop for GpuInput {
    fn drop(&mut self) {
        // SAFETY: created in setup_gpu_input(); FFmpeg's references are
        // gone once a packet is out (encode() waits for it).
        unsafe { ffi::av_buffer_unref(&mut self.frame_buf) };
    }
}

/// FFmpeg's free callback for memory it must not free.
unsafe extern "C" fn keep_memory(_opaque: *mut std::ffi::c_void, _data: *mut u8) {}

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
            let mut enc = Self {
                device: ptr::null_mut(),
                frames: ptr::null_mut(),
                ctx: ptr::null_mut(),
                sw: ffi::av_frame_alloc(),
                hw: ffi::av_frame_alloc(),
                pkt: ffi::av_packet_alloc(),
                codec: cfg.codec,
                width: cfg.width,
                height: cfg.height,
                fps: cfg.fps,
                intra_refresh: cfg.intra_refresh,
                gpu_input: None,
                force_keyframe: true,
                rate_change: None,
                next_pts: 0,
            };
            if enc.sw.is_null() || enc.hw.is_null() || enc.pkt.is_null() {
                return Err(CodecError::Backend("out of memory".into()));
            }
            let gpu = CString::new(cfg.gpu.to_string()).expect("digits");
            check(
                "open CUDA device",
                ffi::av_hwdevice_ctx_create(
                    &mut enc.device,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                    gpu.as_ptr(),
                    ptr::null_mut(),
                    0,
                ),
            )?;
            enc.frames = ffi::av_hwframe_ctx_alloc(enc.device);
            if enc.frames.is_null() {
                return Err(CodecError::Backend("av_hwframe_ctx_alloc failed".into()));
            }
            let fc = (*enc.frames).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
            (*fc).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            (*fc).width = cfg.width as c_int;
            (*fc).height = cfg.height as c_int;
            (*fc).initial_pool_size = 4;
            check("init CUDA frame pool", ffi::av_hwframe_ctx_init(enc.frames))?;
            enc.ctx = enc.open_codec(cfg.bitrate_kbps)?;

            (*enc.sw).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            (*enc.sw).width = cfg.width as c_int;
            (*enc.sw).height = cfg.height as c_int;
            check("allocate upload frame", ffi::av_frame_get_buffer(enc.sw, 0))?;
            Ok(enc)
        }
    }

    /// The codec context: CBR at `kbps`, on this encoder's CUDA frame pool.
    ///
    /// # Safety
    /// `self.frames` is an initialised CUDA frame pool.
    unsafe fn open_codec(&self, kbps: u32) -> Result<*mut ffi::AVCodecContext, CodecError> {
        let names = ff::names(self.codec, ff::Backend::Nvenc)?;
        let mut opts = vec![
            (c"preset", c"p1"),
            (c"tune", c"ull"),
            (c"rc", c"cbr"),
            (c"zerolatency", c"1"),
            (c"delay", c"0"),
            (c"rc-lookahead", c"0"),
            (c"forced-idr", c"1"),
            // A keyframe may be no larger than other frames (ULL's default,
            // stated: no burst on the link when one is forced).
            (c"ldkfs", c"1"),
        ];
        if self.intra_refresh {
            opts.push((c"intra-refresh", c"1"));
        }
        if let Some(profile) = names.profile {
            opts.push((c"profile", profile));
        }
        let (width, height, fps, frames) = (self.width, self.height, self.fps, self.frames);
        // With intra refresh FFmpeg takes the GOP length as the refresh
        // period (and makes the GOP endless itself).
        let gop = if self.intra_refresh {
            ((f64::from(fps) * REFRESH_PERIOD_S).round() as c_int).max(2)
        } else {
            c_int::MAX
        };
        // SAFETY: open_encoder hands over a fresh context; frames is valid
        // per the contract.
        unsafe {
            ff::open_encoder(names.encoder, &opts, |c| {
                (*c).width = width as c_int;
                (*c).height = height as c_int;
                (*c).time_base = ffi::AVRational {
                    num: 1,
                    den: fps as c_int,
                };
                (*c).framerate = ffi::AVRational {
                    num: fps as c_int,
                    den: 1,
                };
                (*c).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
                (*c).sw_pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
                (*c).hw_frames_ctx = ffi::av_buffer_ref(frames);
                (*c).max_b_frames = 0;
                (*c).gop_size = gop;
                ff::set_cbr(c, kbps, fps);
                (*c).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as c_int;
                signal_bt709_limited(c);
            })
        }
    }

    /// A new codec context at `kbps` on the same CUDA context, frame pool
    /// and screen input; it starts with a keyframe.
    fn reopen(&mut self, kbps: u32) -> Result<(), CodecError> {
        // SAFETY: frames is initialised (new() succeeded); no frame is in
        // flight between encode() calls, so the old context can go.
        unsafe {
            let ctx = self.open_codec(kbps)?;
            ffi::avcodec_free_context(&mut self.ctx);
            self.ctx = ctx;
        }
        self.force_keyframe = true;
        Ok(())
    }

    /// FFmpeg's CUDA context and stream for this encoder.
    fn cuda(&self) -> (cuda::CuContext, *mut std::ffi::c_void) {
        // SAFETY: device is the encoder's CUDA device context.
        unsafe { cuda_of(self.device) }
    }

    fn setup_gpu_input(&self) -> Result<GpuInput, CodecError> {
        let backend = |e: String| CodecError::Backend(e);
        let (ctx, _) = self.cuda();
        let cuda_uuid = cuda::Current::push(ctx)?.device_uuid()?;
        // The Vulkan device on the GPU NVENC runs on: its memory goes to
        // CUDA without a copy.
        let gpu = Arc::new(Gpu::preferring(Prefer::Uuid(cuda_uuid)).map_err(backend)?);
        if cuda_uuid != gpu.device_uuid() {
            return Err(CodecError::Backend(format!(
                "Vulkan picked {}, which is not the GPU NVENC runs on; choose it with {}",
                gpu.name(),
                fernsicht_gpu::DEVICE_ENV
            )));
        }
        let name = gpu.name().to_string();
        let converter = Converter::new(gpu, self.width, self.height).map_err(backend)?;
        let fd = converter.export_output().map_err(backend)?;
        let buffer = cuda::ImportedBuffer::import(
            ctx,
            fd,
            converter.allocation_size(),
            converter.layout().len(),
        )?;
        // SAFETY: wraps the imported memory for FFmpeg without handing it
        // over (keep_memory frees nothing).
        let frame_buf = unsafe {
            ffi::av_buffer_create(
                buffer.ptr as *mut u8,
                converter.layout().len() as usize,
                Some(keep_memory),
                ptr::null_mut(),
                0,
            )
        };
        if frame_buf.is_null() {
            return Err(CodecError::Backend("out of memory".into()));
        }
        log::info!("NVENC takes the screen from Vulkan on {name} without a CPU copy");
        Ok(GpuInput {
            frame_buf,
            buffer,
            converter,
        })
    }

    /// DMA-BUF → NV12 on the GPU, into the buffer `self.hw` then points
    /// at: NVENC reads it as it is.
    unsafe fn convert_dmabuf(&mut self, image: &DmaBuf) -> Result<(), CodecError> {
        if self.gpu_input.is_none() {
            self.gpu_input = Some(self.setup_gpu_input().map_err(|e| e.to_string()));
        }
        let input = match self.gpu_input.as_mut().expect("set above") {
            Ok(input) => input,
            // Set up once; the same error for every frame.
            Err(e) => return Err(CodecError::Backend(format!("screen input for NVENC: {e}"))),
        };
        // Waits for Vulkan: the picture is complete when NVENC reads it.
        if let Err(e) = input.converter.convert(image) {
            return Err(if input.converter.gpu_lost() {
                CodecError::GpuLost
            } else {
                CodecError::Backend(e)
            });
        }
        let l = input.converter.layout();
        // SAFETY: hw is an empty frame (encode() unrefs it); the buffer
        // outlives the references FFmpeg takes (dropped with the packet).
        unsafe {
            let buf = ffi::av_buffer_ref(input.frame_buf);
            let frames = ffi::av_buffer_ref(self.frames);
            let f = &mut *self.hw;
            f.buf[0] = buf;
            f.hw_frames_ctx = frames;
            if buf.is_null() || frames.is_null() {
                ffi::av_frame_unref(self.hw);
                return Err(CodecError::Backend("out of memory".into()));
            }
            f.format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA as c_int;
            f.width = self.width as c_int;
            f.height = self.height as c_int;
            // NVENC finds UV at pitch × height after Y, where it is.
            f.data[0] = input.buffer.ptr as *mut u8;
            f.data[1] = (input.buffer.ptr + l.uv_offset) as *mut u8;
            f.linesize[0] = l.pitch as c_int;
            f.linesize[1] = l.pitch as c_int;
        }
        Ok(())
    }

    /// CPU NV12 → `self.hw`.
    unsafe fn upload(&mut self, frame: &Frame) -> Result<(), CodecError> {
        if frame.format != PixelFormat::Nv12
            || frame.width != self.width
            || frame.height != self.height
            || frame.data.len() < PixelFormat::Nv12.frame_bytes(self.width, self.height)
        {
            return Err(CodecError::Backend(
                "input must be NV12 at the configured size".into(),
            ));
        }
        // SAFETY: sw was allocated for this size in new(); hw is an empty
        // frame (encode() unrefs it).
        unsafe {
            check(
                "get CUDA surface",
                ffi::av_hwframe_get_buffer(self.frames, self.hw, 0),
            )?;
            check(
                "make upload frame writable",
                ffi::av_frame_make_writable(self.sw),
            )?;
            nv12_into_frame(
                &frame.data,
                self.sw,
                self.width as usize,
                self.height as usize,
            );
            check(
                "upload to CUDA",
                ffi::av_hwframe_transfer_data(self.hw, self.sw, 0),
            )?;
        }
        Ok(())
    }
}

impl Encoder for NvencEncoder {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn encode(&mut self, frame: &Frame, out: &mut EncodedFrame) -> Result<(), CodecError> {
        out.data.clear();
        out.keyframe = false;
        // SAFETY: all pointers were created in new() and stay valid.
        unsafe {
            ffi::av_frame_unref(self.hw);
            match &frame.dmabuf {
                Some(image) => self.convert_dmabuf(image)?,
                None => self.upload(frame)?,
            }
            let f = &mut *self.hw;
            f.pts = self.next_pts;
            self.next_pts += 1;
            // Right after a bitrate change, a keyframe can only be the IDR
            // frame of FFmpeg's reconfiguration: none is forced then (the
            // reconfiguration brings one, or the reopened encoder does).
            let force = self.force_keyframe && self.rate_change.is_none();
            f.pict_type = if force {
                // With forced-idr this is an IDR frame.
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            check("send frame", ffi::avcodec_send_frame(self.ctx, self.hw))?;
            if force {
                self.force_keyframe = false;
            }
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
        if out.keyframe {
            // Whatever made it, a keyframe answers a pending request.
            self.force_keyframe = false;
        }
        if let Some(kbps) = self.rate_change.take()
            && !out.keyframe
        {
            // No IDR frame: FFmpeg did not reconfigure (this GPU cannot
            // change the rate of a running session). A new codec context
            // can, on the same GPU objects.
            log::info!("NVENC changes the bitrate by reopening the encoder on this GPU");
            self.reopen(kbps)?;
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

    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError> {
        // FFmpeg reconfigures the running NVENC session with the next frame
        // (NvEncReconfigureEncoder; it forces an IDR frame). encode() checks
        // that it did.
        // SAFETY: ctx is the open context.
        unsafe { ff::set_cbr(self.ctx, kbps, self.fps) };
        self.rate_change = Some(kbps);
        Ok(())
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        // CUDA and Vulkan objects first, while the CUDA context lives.
        self.gpu_input = None;
        // SAFETY: each pointer is either null or owned by us.
        unsafe {
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_frame_free(&mut self.sw);
            ffi::av_frame_free(&mut self.hw);
            ffi::av_packet_free(&mut self.pkt);
            ffi::av_buffer_unref(&mut self.frames);
            ffi::av_buffer_unref(&mut self.device);
        }
    }
}

/// Whether NVENC on CUDA device `gpu` encodes `codec`: a small encoder is
/// opened and closed again (some GPUs lack AV1 or HEVC). False without the
/// NVIDIA driver.
pub fn encodes(gpu: u32, codec: Codec) -> bool {
    codec != Codec::Synthetic
        && NvencEncoder::new(&NvencConfig {
            gpu,
            codec,
            width: 256,
            height: 256,
            fps: 30,
            bitrate_kbps: 1_000,
            intra_refresh: false,
        })
        .is_ok()
}

/// Whether NVDEC on CUDA device `gpu` decodes `codec` (H.264, HEVC or AV1,
/// 8 bit, up to at least 1080p). False without the NVIDIA driver.
pub fn decodes(gpu: u32, codec: Codec) -> bool {
    let cuvid = match codec {
        Codec::H264 => cuda::CUVID_H264,
        Codec::Hevc => cuda::CUVID_HEVC,
        Codec::Av1 => cuda::CUVID_AV1,
        Codec::Synthetic => return false,
    };
    let index = CString::new(gpu.to_string()).expect("digits");
    let mut device = ptr::null_mut();
    // SAFETY: creates and releases a CUDA device context.
    unsafe {
        if ffi::av_hwdevice_ctx_create(
            &mut device,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            index.as_ptr(),
            ptr::null_mut(),
            0,
        ) < 0
        {
            return false;
        }
        let dc = (*device).data as *mut ffi::AVHWDeviceContext;
        let ctx = (*((*dc).hwctx as *const ffi::AVCUDADeviceContext)).cuda_ctx;
        let ok = cuda::Current::push(ctx.cast())
            .and_then(|c| cuda::nvdec_supports(&c, cuvid))
            .unwrap_or_else(|e| {
                log::debug!("NVDEC capability query: {e}");
                false
            });
        ffi::av_buffer_unref(&mut device);
        ok
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

/// FFmpeg's CUDA context and stream of a CUDA device context.
///
/// # Safety
/// `device` is a CUDA `AVHWDeviceContext` reference.
unsafe fn cuda_of(device: *mut ffi::AVBufferRef) -> (cuda::CuContext, *mut std::ffi::c_void) {
    // SAFETY: a CUDA device context's hwctx is an AVCUDADeviceContext.
    unsafe {
        let dc = (*device).data as *mut ffi::AVHWDeviceContext;
        let cu = &*((*dc).hwctx as *const ffi::AVCUDADeviceContext);
        (cu.cuda_ctx.cast(), cu.stream.cast())
    }
}

/// Where NVDEC's pictures go for the renderer: CUDA memory exported as a
/// file descriptor, each picture copied into it on the GPU.
struct Export {
    buffer: cuda::ExportedBuffer,
    device_uuid: [u8; 16],
    /// Unique in the process: a renderer's import of another id is stale.
    id: u64,
    width: u32,
    height: u32,
    /// Row pitch of both planes, bytes.
    pitch: u32,
}

impl Export {
    fn new(ctx: cuda::CuContext, width: u32, height: u32) -> Result<Self, CodecError> {
        static IDS: AtomicU64 = AtomicU64::new(1);
        // Rows start on 256 bytes, as copies into images like them.
        let pitch = width.next_multiple_of(256);
        let len = pitch as usize * height as usize * 3 / 2;
        let buffer = cuda::ExportedBuffer::new(ctx, len)?;
        let device_uuid = cuda::Current::push(ctx)?.device_uuid()?;
        Ok(Self {
            buffer,
            device_uuid,
            id: IDS.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            pitch,
        })
    }

    fn picture(&self) -> Picture<'_> {
        let rows = u64::from(self.pitch) * u64::from(self.height);
        Picture::Exported {
            memory: self.buffer.fd(),
            size: self.buffer.size as u64,
            id: self.id,
            device_uuid: self.device_uuid,
            width: self.width,
            height: self.height,
            planes: [(0, self.pitch), (rows, self.pitch)],
        }
    }
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
    /// Set up on the first request for a GPU picture; an error is kept
    /// (and pictures go through the CPU).
    export: Option<Result<Export, String>>,
}

// SAFETY: as for NvencEncoder.
unsafe impl Send for NvdecDecoder {}

impl NvdecDecoder {
    /// An H.264 decoder; `gpu`: CUDA device index.
    pub fn new(gpu: u32) -> Result<Self, CodecError> {
        Self::for_codec(gpu, Codec::H264)
    }

    /// A decoder for `codec` (H.264, HEVC or AV1).
    pub fn for_codec(gpu: u32, codec: Codec) -> Result<Self, CodecError> {
        let names = ff::names(codec, ff::Backend::Nvenc)?;
        let index = CString::new(gpu.to_string()).expect("digits");
        // SAFETY: as in NvencEncoder::new.
        unsafe {
            let codec = ffi::avcodec_find_decoder_by_name(names.decoder.as_ptr());
            if codec.is_null() {
                return Err(CodecError::Backend(format!(
                    "FFmpeg has no {} decoder",
                    names.label
                )));
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
                export: None,
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
                "open the decoder",
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

    /// Copies the last decoded `width`×`height` frame into the exported
    /// memory, on the GPU; sets it up first (again for a new size).
    fn export(&mut self, width: u32, height: u32) -> Result<(), CodecError> {
        // SAFETY: device is the decoder's CUDA device context.
        let (ctx, stream) = unsafe { cuda_of(self.device) };
        if self.export.as_ref().is_none_or(|e| {
            e.as_ref()
                .is_ok_and(|e| (e.width, e.height) != (width, height))
        }) {
            self.export = None;
            let e = Export::new(ctx, width, height).map_err(|e| e.to_string());
            if let Err(e) = &e {
                log::info!("NVDEC pictures go through the CPU: {e}");
            }
            self.export = Some(e);
        }
        let e = match &self.export {
            Some(Ok(e)) => e,
            Some(Err(e)) => return Err(CodecError::Backend(e.clone())),
            None => unreachable!("set above"),
        };
        // SAFETY: frame holds a decoded CUDA NV12 frame of width×height
        // (checked by the caller): two planes of device memory.
        let (y, y_pitch, uv, uv_pitch) = unsafe {
            let f = &*self.frame;
            (
                f.data[0] as cuda::CuDevicePtr,
                f.linesize[0] as usize,
                f.data[1] as cuda::CuDevicePtr,
                f.linesize[1] as usize,
            )
        };
        let (w, h, pitch) = (width as usize, height as usize, e.pitch as usize);
        let current = cuda::Current::push(ctx)?;
        // On FFmpeg's stream: after NVDEC's own copy into the frame.
        cuda::copy_2d(
            &current,
            stream,
            &[
                cuda::Copy2D {
                    src: y,
                    src_pitch: y_pitch,
                    dst: e.buffer.ptr,
                    dst_pitch: pitch,
                    width_bytes: w,
                    rows: h,
                },
                cuda::Copy2D {
                    src: uv,
                    src_pitch: uv_pitch,
                    dst: e.buffer.ptr + (pitch * h) as u64,
                    dst_pitch: pitch,
                    width_bytes: w,
                    rows: h / 2,
                },
            ],
        )
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
                    CodecError::Corrupt("invalid video data")
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

    /// A GPU picture copied on the GPU into exported CUDA memory where the
    /// presenter wants one and CUDA can export, else NV12 in CPU memory.
    fn picture(&mut self, prefer: PictureKind) -> Result<Option<Picture<'_>>, CodecError> {
        // SAFETY: frame is a valid AVFrame.
        let (format, width, height) = unsafe {
            let f = &*self.frame;
            (f.format, f.width as u32, f.height as u32)
        };
        if format != ffi::AVPixelFormat::AV_PIX_FMT_CUDA as c_int {
            return Ok(None);
        }
        let exportable = !matches!(self.export, Some(Err(_)));
        if prefer == PictureKind::Gpu && exportable && self.export(width, height).is_ok() {
            let e = self.export.as_ref().and_then(|e| e.as_ref().ok());
            return Ok(e.map(Export::picture));
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
        // The exported memory first, while the CUDA context lives (fields
        // drop only after this).
        self.export = None;
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
