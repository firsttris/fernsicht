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
//! Input is either NV12 in CPU memory, uploaded per frame, or an RGB
//! DMA-BUF (KMS capture). A DMA-BUF is imported without a copy and
//! converted to NV12 (BT.709, limited range) and scaled to the stream size
//! by the GPU's video processor (`scale_vaapi`).

use std::ffi::{CStr, CString, c_int};
use std::ptr;

use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

use fernsicht_capture::dmabuf::{DmaBuf, DmaBufPlane, formats, fourcc_name};
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
    fn vaCreateSurfaces(
        dpy: ffi::VADisplay,
        format: u32,
        width: u32,
        height: u32,
        surfaces: *mut ffi::VASurfaceID,
        num_surfaces: u32,
        attrib_list: *mut VaSurfaceAttrib,
        num_attribs: u32,
    ) -> c_int;
    fn vaDestroySurfaces(
        dpy: ffi::VADisplay,
        surfaces: *mut ffi::VASurfaceID,
        num_surfaces: c_int,
    ) -> c_int;
}

// libva types FFmpeg's bindings leave opaque (va.h, va_drmcommon.h).
#[repr(C)]
#[derive(Clone, Copy)]
union VaGenericValueUnion {
    i: i32,
    p: *mut std::ffi::c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaGenericValue {
    kind: c_int,
    value: VaGenericValueUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VaSurfaceAttrib {
    kind: c_int,
    flags: u32,
    value: VaGenericValue,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VaDrmObject {
    fd: c_int,
    size: u32,
    drm_format_modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VaDrmLayer {
    drm_format: u32,
    num_planes: u32,
    object_index: [u32; 4],
    offset: [u32; 4],
    pitch: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VaDrmPrimeSurfaceDescriptor {
    fourcc: u32,
    width: u32,
    height: u32,
    num_objects: u32,
    objects: [VaDrmObject; 4],
    num_layers: u32,
    layers: [VaDrmLayer; 4],
}

const VA_SURFACE_ATTRIB_SETTABLE: u32 = 2;
const VA_SURFACE_ATTRIB_MEMORY_TYPE: c_int = 6;
const VA_SURFACE_ATTRIB_EXTERNAL_BUFFER_DESCRIPTOR: c_int = 7;
const VA_GENERIC_VALUE_INTEGER: c_int = 1;
const VA_GENERIC_VALUE_POINTER: c_int = 3;
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: i32 = 0x4000_0000;
const VA_RT_FORMAT_RGB32: u32 = 0x0002_0000;
const VA_RT_FORMAT_RGB32_10: u32 = 0x0020_0000;
const VA_FOURCC_BGRX: u32 = 0x5852_4742;
const VA_FOURCC_RGBX: u32 = 0x5842_4752;
const VA_FOURCC_X2R10G10B10: u32 = 0x3033_5258;
const VA_FOURCC_X2B10G10R10: u32 = 0x3033_4258;

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
    fps: u32,
    /// Built on the first DMA-BUF frame, rebuilt when its geometry changes.
    converter: Option<GpuConverter>,
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
                fps: cfg.fps,
                converter: None,
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

impl VaapiEncoder {
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
        // SAFETY: see encode().
        unsafe {
            self.fill_sw(frame)?;
            check(
                "get VAAPI surface",
                ffi::av_hwframe_get_buffer(self.frames, self.hw, 0),
            )?;
            check(
                "upload to VAAPI surface",
                ffi::av_hwframe_transfer_data(self.hw, self.sw, 0),
            )?;
        }
        Ok(())
    }

    /// RGB DMA-BUF → NV12 surface in `self.hw`, all on the GPU.
    unsafe fn convert_dmabuf(&mut self, image: &DmaBuf) -> Result<(), CodecError> {
        image
            .validate()
            .map_err(|e| CodecError::Backend(e.to_string()))?;
        let key = (image.fourcc, image.width, image.height);
        if self.converter.as_ref().is_none_or(|c| c.key != key) {
            self.converter = None;
            self.converter = Some(GpuConverter::new(
                &self._device,
                key,
                (self.width, self.height),
                self.fps,
            )?);
        }
        let conv = self.converter.as_mut().expect("just built");
        // SAFETY: hw is an empty frame owned by us.
        unsafe { conv.convert(image, self.hw) }
    }
}

impl Encoder for VaapiEncoder {
    fn codec(&self) -> Codec {
        Codec::H264
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
        self.converter = None;
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

/// How a DRM format is imported into VAAPI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImportFormat {
    /// The format without alpha: scanout ignores alpha, and so do we.
    drm: u32,
    va_fourcc: u32,
    rt_format: u32,
    /// A format of the same depth for FFmpeg's bookkeeping; the driver
    /// takes the real layout from the imported surface.
    sw_format: ffi::AVPixelFormat,
}

/// Formats KMS and PipeWire hand out for desktops: 8 bit, and the 10 bit
/// that KDE Plasma uses on AMD (`AB30`).
fn import_format(fourcc: u32) -> Result<ImportFormat, CodecError> {
    use ffi::AVPixelFormat::{AV_PIX_FMT_BGR0, AV_PIX_FMT_X2RGB10LE};
    let (drm, va_fourcc, rt_format, sw_format) = match fourcc {
        formats::XRGB8888 | formats::ARGB8888 => (
            formats::XRGB8888,
            VA_FOURCC_BGRX,
            VA_RT_FORMAT_RGB32,
            AV_PIX_FMT_BGR0,
        ),
        formats::XBGR8888 | formats::ABGR8888 => (
            formats::XBGR8888,
            VA_FOURCC_RGBX,
            VA_RT_FORMAT_RGB32,
            AV_PIX_FMT_BGR0,
        ),
        formats::XRGB2101010 | formats::ARGB2101010 => (
            formats::XRGB2101010,
            VA_FOURCC_X2R10G10B10,
            VA_RT_FORMAT_RGB32_10,
            AV_PIX_FMT_X2RGB10LE,
        ),
        formats::XBGR2101010 | formats::ABGR2101010 => (
            formats::XBGR2101010,
            VA_FOURCC_X2B10G10R10,
            VA_RT_FORMAT_RGB32_10,
            AV_PIX_FMT_X2RGB10LE,
        ),
        other => {
            return Err(CodecError::Backend(format!(
                "DMA-BUF format {} is not supported",
                fourcc_name(other)
            )));
        }
    };
    Ok(ImportFormat {
        drm,
        va_fourcc,
        rt_format,
        sw_format,
    })
}

/// An imported surface, destroyed when FFmpeg drops the last reference.
struct ImportedSurface {
    display: ffi::VADisplay,
    id: ffi::VASurfaceID,
}

unsafe extern "C" fn release_surface(_opaque: *mut std::ffi::c_void, data: *mut u8) {
    // SAFETY: data came from Box::into_raw in GpuConverter::import.
    let mut s = unsafe { Box::from_raw(data as *mut ImportedSurface) };
    // SAFETY: the surface was created by vaCreateSurfaces on this display
    // and nothing references it any more.
    unsafe { vaDestroySurfaces(s.display, &mut s.id, 1) };
}

/// Imports RGB DMA-BUFs as VAAPI surfaces and converts them to NV12 at the
/// stream size with the video processor: `buffer → scale_vaapi → sink`.
struct GpuConverter {
    /// (fourcc, width, height) of the input this was built for.
    key: (u32, u32, u32),
    /// Frames context the imported surfaces belong to (no own pool).
    import_frames: *mut ffi::AVBufferRef,
    graph: *mut ffi::AVFilterGraph,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    display: ffi::VADisplay,
    format: ImportFormat,
    input: *mut ffi::AVFrame,
    next_pts: i64,
}

impl GpuConverter {
    fn new(
        device: &VaapiDevice,
        key: (u32, u32, u32),
        (out_w, out_h): (u32, u32),
        fps: u32,
    ) -> Result<Self, CodecError> {
        let (fourcc, w, h) = key;
        let format = import_format(fourcc)?;
        // SAFETY: as in VaapiEncoder::new, pointers are checked and Drop
        // handles a partly built value.
        unsafe {
            let mut conv = Self {
                key,
                import_frames: ffi::av_hwframe_ctx_alloc(device.ctx),
                graph: ffi::avfilter_graph_alloc(),
                src: ptr::null_mut(),
                sink: ptr::null_mut(),
                display: device.display(),
                format,
                input: ffi::av_frame_alloc(),
                next_pts: 0,
            };
            if conv.import_frames.is_null() || conv.graph.is_null() || conv.input.is_null() {
                return Err(CodecError::Backend("out of memory".into()));
            }
            let fc = (*conv.import_frames).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = format.sw_format;
            (*fc).width = w as c_int;
            (*fc).height = h as c_int;
            check(
                "init VAAPI import context",
                ffi::av_hwframe_ctx_init(conv.import_frames),
            )?;

            let g = conv.graph;
            conv.src = ffi::avfilter_graph_alloc_filter(
                g,
                ffi::avfilter_get_by_name(c"buffer".as_ptr()),
                c"in".as_ptr(),
            );
            let scale = ffi::avfilter_graph_alloc_filter(
                g,
                ffi::avfilter_get_by_name(c"scale_vaapi".as_ptr()),
                c"convert".as_ptr(),
            );
            conv.sink = ffi::avfilter_graph_alloc_filter(
                g,
                ffi::avfilter_get_by_name(c"buffersink".as_ptr()),
                c"out".as_ptr(),
            );
            if conv.src.is_null() || scale.is_null() || conv.sink.is_null() {
                return Err(CodecError::Backend(
                    "FFmpeg lacks the buffer, scale_vaapi or buffersink filter".into(),
                ));
            }

            let par = ffi::av_buffersrc_parameters_alloc();
            if par.is_null() {
                return Err(CodecError::Backend("out of memory".into()));
            }
            (*par).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
            (*par).width = w as c_int;
            (*par).height = h as c_int;
            (*par).time_base = ffi::AVRational {
                num: 1,
                den: fps as c_int,
            };
            (*par).sample_aspect_ratio = ffi::AVRational { num: 1, den: 1 };
            (*par).hw_frames_ctx = conv.import_frames;
            let r = ffi::av_buffersrc_parameters_set(conv.src, par);
            ffi::av_free(par as *mut _);
            check("configure buffer source", r)?;
            check(
                "init buffer source",
                ffi::avfilter_init_str(conv.src, ptr::null()),
            )?;

            let args = CString::new(format!(
                "w={out_w}:h={out_h}:format=nv12:out_color_matrix=bt709:out_range=tv"
            ))
            .expect("no NUL");
            check(
                "init scale_vaapi",
                ffi::avfilter_init_str(scale, args.as_ptr()),
            )?;
            check(
                "init buffer sink",
                ffi::avfilter_init_str(conv.sink, ptr::null()),
            )?;
            check("link source", ffi::avfilter_link(conv.src, 0, scale, 0))?;
            check("link sink", ffi::avfilter_link(scale, 0, conv.sink, 0))?;
            check(
                "configure filter graph",
                ffi::avfilter_graph_config(g, ptr::null_mut()),
            )?;
            Ok(conv)
        }
    }

    /// Imports `image` as a VAAPI surface (no copy) into `self.input`.
    unsafe fn import(&mut self, image: &DmaBuf) -> Result<(), CodecError> {
        let mut desc = VaDrmPrimeSurfaceDescriptor {
            fourcc: self.format.va_fourcc,
            width: image.width,
            height: image.height,
            num_objects: image.objects.len() as u32,
            num_layers: 1,
            ..Default::default()
        };
        for (o, fd) in desc.objects.iter_mut().zip(&image.objects) {
            *o = VaDrmObject {
                fd: fd.as_raw_fd(),
                size: u32::try_from(object_size(fd)?)
                    .map_err(|_| CodecError::Backend("DMA-BUF larger than 4 GiB".into()))?,
                drm_format_modifier: image.modifier,
            };
        }
        let layer = &mut desc.layers[0];
        layer.drm_format = self.format.drm;
        layer.num_planes = image.planes.len() as u32;
        for (i, p) in image.planes.iter().enumerate() {
            layer.object_index[i] = p.object as u32;
            layer.offset[i] = p.offset;
            layer.pitch[i] = p.pitch;
        }
        let mut attribs = [
            VaSurfaceAttrib {
                kind: VA_SURFACE_ATTRIB_MEMORY_TYPE,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VaGenericValue {
                    kind: VA_GENERIC_VALUE_INTEGER,
                    value: VaGenericValueUnion {
                        i: VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                    },
                },
            },
            VaSurfaceAttrib {
                kind: VA_SURFACE_ATTRIB_EXTERNAL_BUFFER_DESCRIPTOR,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                value: VaGenericValue {
                    kind: VA_GENERIC_VALUE_POINTER,
                    value: VaGenericValueUnion {
                        p: (&raw mut desc).cast(),
                    },
                },
            },
        ];
        let mut id: ffi::VASurfaceID = 0;
        // SAFETY: the descriptor borrows the file descriptors in `image`
        // only for this call; the driver takes its own reference to the
        // memory.
        let r = unsafe {
            vaCreateSurfaces(
                self.display,
                self.format.rt_format,
                image.width,
                image.height,
                &mut id,
                1,
                attribs.as_mut_ptr(),
                attribs.len() as u32,
            )
        };
        if r != 0 {
            return Err(CodecError::Backend(format!(
                "the driver cannot import this DMA-BUF ({} {}×{}, modifier {:#x}): VA error {r}",
                fourcc_name(image.fourcc),
                image.width,
                image.height,
                image.modifier
            )));
        }
        let surface = Box::into_raw(Box::new(ImportedSurface {
            display: self.display,
            id,
        }));
        // SAFETY: `input` is ours; the buffer owns the surface from here on.
        unsafe {
            let buf = ffi::av_buffer_create(
                surface.cast(),
                std::mem::size_of::<ImportedSurface>(),
                Some(release_surface),
                ptr::null_mut(),
                0,
            );
            if buf.is_null() {
                release_surface(ptr::null_mut(), surface.cast());
                return Err(CodecError::Backend("out of memory".into()));
            }
            ffi::av_frame_unref(self.input);
            let f = &mut *self.input;
            f.buf[0] = buf;
            f.data[3] = id as usize as *mut u8;
            f.format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
            f.width = image.width as c_int;
            f.height = image.height as c_int;
            f.hw_frames_ctx = ffi::av_buffer_ref(self.import_frames);
            // A desktop: full-range sRGB.
            f.color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            f.colorspace = ffi::AVColorSpace::AVCOL_SPC_RGB;
            f.color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            f.color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1;
            f.pts = self.next_pts;
        }
        self.next_pts += 1;
        Ok(())
    }

    /// Imports `image` and writes the converted NV12 surface into `out`.
    unsafe fn convert(&mut self, image: &DmaBuf, out: *mut ffi::AVFrame) -> Result<(), CodecError> {
        if image.planes.len() > 4 || image.objects.len() > 4 {
            return Err(CodecError::Backend("too many DMA-BUF planes".into()));
        }
        // SAFETY: input and out are valid frames; the graph takes our
        // reference to the imported surface (and resets `input`).
        unsafe {
            self.import(image)?;
            check(
                "feed video processor",
                ffi::av_buffersrc_add_frame_flags(self.src, self.input, 0),
            )?;
            check(
                "convert to NV12",
                ffi::av_buffersink_get_frame(self.sink, out),
            )?;
        }
        Ok(())
    }
}

/// Size of a DMA-BUF object; seeking to the end reports it.
fn object_size(fd: &OwnedFd) -> Result<usize, CodecError> {
    // SAFETY: lseek on a valid descriptor; DMA-BUFs support SEEK_END.
    let end = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
    if end <= 0 {
        return Err(CodecError::Backend(
            "descriptor is not a DMA-BUF (cannot get its size)".into(),
        ));
    }
    // SAFETY: as above; rewinding keeps the descriptor as we found it.
    unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_SET) };
    Ok(end as usize)
}

impl Drop for GpuConverter {
    fn drop(&mut self) {
        // SAFETY: each pointer is either null or owned by us; the graph
        // frees its filters.
        unsafe {
            ffi::avfilter_graph_free(&mut self.graph);
            ffi::av_frame_free(&mut self.input);
            ffi::av_buffer_unref(&mut self.import_frames);
        }
    }
}

/// Puts a CPU image on the GPU and hands it out as a DMA-BUF, the way KMS
/// capture delivers frames. `pixels` are 4 bytes per pixel, tightly packed,
/// in the memory layout of the DRM `fourcc` (8 or 10 bit RGB). For tests and
/// diagnostics: it exercises the zero-copy import without needing the
/// privileges of KMS capture.
pub fn upload_as_dmabuf(
    render_node: &str,
    width: u32,
    height: u32,
    fourcc: u32,
    pixels: &[u8],
) -> Result<DmaBuf, CodecError> {
    // The bytes are copied as they are, so any format of the same depth
    // works as storage; the returned image carries the real fourcc.
    let storage = import_format(fourcc)?.sw_format;
    let bgrx = pixels;
    if width == 0 || height == 0 || bgrx.len() < width as usize * height as usize * 4 {
        return Err(CodecError::Backend(
            "image smaller than width×height×4".into(),
        ));
    }
    let device = VaapiDevice::open(render_node)?;
    let (w, h) = (width as usize, height as usize);
    // SAFETY: every FFmpeg object below is checked and freed before return.
    unsafe {
        let mut frames = ffi::av_hwframe_ctx_alloc(device.ctx);
        let mut sw = ffi::av_frame_alloc();
        let mut hw = ffi::av_frame_alloc();
        let mut drm = ffi::av_frame_alloc();
        let result = (|| {
            if frames.is_null() || sw.is_null() || hw.is_null() || drm.is_null() {
                return Err(CodecError::Backend("out of memory".into()));
            }
            let fc = (*frames).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = storage;
            (*fc).width = width as c_int;
            (*fc).height = height as c_int;
            (*fc).initial_pool_size = 1;
            check("init RGB frame pool", ffi::av_hwframe_ctx_init(frames))?;

            (*sw).format = storage as c_int;
            (*sw).width = width as c_int;
            (*sw).height = height as c_int;
            check("allocate RGB frame", ffi::av_frame_get_buffer(sw, 0))?;
            for row in 0..h {
                let dst = (*sw).data[0].add(row * (*sw).linesize[0] as usize);
                ptr::copy_nonoverlapping(bgrx.as_ptr().add(row * w * 4), dst, w * 4);
            }
            check("get surface", ffi::av_hwframe_get_buffer(frames, hw, 0))?;
            check("upload RGB", ffi::av_hwframe_transfer_data(hw, sw, 0))?;

            (*drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            check(
                "export surface as DMA-BUF",
                ffi::av_hwframe_map(drm, hw, ffi::AV_HWFRAME_MAP_READ as c_int),
            )?;
            let desc = &*((*drm).data[0] as *const ffi::AVDRMFrameDescriptor);
            // The export closes its descriptors when unmapped; keep copies.
            let objects = desc.objects[..desc.nb_objects as usize]
                .iter()
                .map(|o| {
                    BorrowedFd::borrow_raw(o.fd)
                        .try_clone_to_owned()
                        .map(Arc::new)
                        .map_err(|e| CodecError::Backend(format!("dup DMA-BUF: {e}")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut planes = Vec::new();
            for layer in &desc.layers[..desc.nb_layers as usize] {
                for p in &layer.planes[..layer.nb_planes as usize] {
                    planes.push(DmaBufPlane {
                        object: p.object_index as usize,
                        offset: p.offset as u32,
                        pitch: p.pitch as u32,
                    });
                }
            }
            Ok(DmaBuf {
                width,
                height,
                fourcc,
                modifier: desc.objects[0].format_modifier,
                objects,
                planes,
            })
        })();
        ffi::av_frame_free(&mut drm);
        ffi::av_frame_free(&mut hw);
        ffi::av_frame_free(&mut sw);
        ffi::av_buffer_unref(&mut frames);
        result
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
