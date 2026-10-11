//! H.264, HEVC and AV1 decoding with Vulkan Video, via FFmpeg, on our own
//! Vulkan device.
//!
//! FFmpeg's Vulkan decoder runs on the device the presenter draws with
//! ([`fernsicht_gpu::Gpu`]): it is handed our instance, device and queues
//! (sharing their locks), decodes into its pool of NV12 images, and the
//! presenter samples the decoded image in place: no copy, no second API.
//! Works wherever the driver has Vulkan Video decode (NVIDIA from Pascal,
//! AMD and Intel with Mesa).
//!
//! Synchronization follows FFmpeg's contract for `AVVkFrame`: a
//! submission that uses the image waits for its timeline semaphore at the
//! current value and signals the next one ([`VkSync`]).

use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ash::vk;
use ash::vk::Handle;
use fernsicht_gpu::Gpu;
use fernsicht_proto::Codec;
use ffmpeg_next::ffi;

use crate::ff::{self, check, eagain, ff_err, is_bt709_limited, nv12_from_frame};
use crate::{CodecError, DecodedFrame, Decoder, Picture, PictureKind, VkAccess, VkPicture, VkSync};

// FFmpeg's VkPhysicalDeviceFeatures2 is filled with ash's.
const _: () = assert!(
    size_of::<ffi::VkPhysicalDeviceFeatures2>() == size_of::<vk::PhysicalDeviceFeatures2>()
);

/// libavcodec of FFmpeg 7.1 (61.19): from there FFmpeg's Vulkan decoding
/// is mature (and it encodes); 6.1 had a first version.
const MATURE_LAVC: u32 = (61 << 16) | (19 << 8);

/// Whether the FFmpeg loaded is new enough for Vulkan Video here.
pub fn ffmpeg_is_ready() -> bool {
    // SAFETY: a plain query.
    unsafe { ffi::avcodec_version() >= MATURE_LAVC }
}

/// Whether `gpu` decodes `codec` with Vulkan Video: the FFmpeg loaded is
/// [ready](ffmpeg_is_ready) and the driver lists the codec on a decode
/// queue (FFmpeg's own support is checked on opening).
pub fn decodes(gpu: &Gpu, codec: Codec) -> bool {
    if !ffmpeg_is_ready() {
        return false;
    }
    let op = match codec {
        Codec::H264 => vk::VideoCodecOperationFlagsKHR::DECODE_H264,
        Codec::Hevc => vk::VideoCodecOperationFlagsKHR::DECODE_H265,
        Codec::Av1 => vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
        Codec::Synthetic => return false,
    };
    gpu.decode
        .as_ref()
        .is_some_and(|q| q.operations.contains(op))
}

/// FFmpeg's Vulkan device context on a [`Gpu`] of ours.
pub struct VulkanDevice {
    buf: *mut ffi::AVBufferRef,
    gpu: Arc<Gpu>,
    /// What the context points into.
    _keep: Box<Keep>,
}

/// The features and extension names FFmpeg is told about; it reads them
/// while the context lives.
struct Keep {
    v11: vk::PhysicalDeviceVulkan11Features<'static>,
    v12: vk::PhysicalDeviceVulkan12Features<'static>,
    v13: vk::PhysicalDeviceVulkan13Features<'static>,
    instance_extensions: Vec<*const c_char>,
    device_extensions: Vec<*const c_char>,
}

// SAFETY: the context is only used by the thread owning this value; the
// queues it submits to are guarded by the Gpu's locks.
unsafe impl Send for VulkanDevice {}

/// FFmpeg locks a queue before it submits to it: the Gpu's lock, which
/// our own submissions take too.
unsafe extern "C" fn lock_queue(ctx: *mut ffi::AVHWDeviceContext, family: u32, _index: u32) {
    // SAFETY: user_opaque is the Gpu, alive as long as the context.
    let gpu = unsafe { &*((*ctx).user_opaque as *const Gpu) };
    if let Some(lock) = gpu.queue_lock_for(family) {
        lock.acquire();
    }
}

unsafe extern "C" fn unlock_queue(ctx: *mut ffi::AVHWDeviceContext, family: u32, _index: u32) {
    // SAFETY: as in lock_queue.
    let gpu = unsafe { &*((*ctx).user_opaque as *const Gpu) };
    if let Some(lock) = gpu.queue_lock_for(family) {
        lock.release();
    }
}

impl VulkanDevice {
    pub fn new(gpu: Arc<Gpu>) -> Result<Self, CodecError> {
        let f = gpu.features;
        if !(f.timeline_semaphore && f.synchronization2) {
            return Err(CodecError::Backend(format!(
                "{} lacks timeline semaphores or synchronization2",
                gpu.name()
            )));
        }
        let mut keep = Box::new(Keep {
            v11: vk::PhysicalDeviceVulkan11Features::default()
                .sampler_ycbcr_conversion(f.sampler_ycbcr_conversion),
            v12: vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true),
            v13: vk::PhysicalDeviceVulkan13Features::default()
                .dynamic_rendering(true)
                .synchronization2(true),
            instance_extensions: gpu.instance_extensions.iter().map(|n| n.as_ptr()).collect(),
            device_extensions: gpu.device_extensions.iter().map(|n| n.as_ptr()).collect(),
        });
        // The chain, inside the box (it does not move).
        keep.v11.p_next = ptr::from_mut(&mut keep.v12).cast();
        keep.v12.p_next = ptr::from_mut(&mut keep.v13).cast();
        let features = vk::PhysicalDeviceFeatures2 {
            p_next: ptr::from_mut(&mut keep.v11).cast(),
            ..Default::default()
        };

        // SAFETY: the context is allocated, filled with handles of the
        // Gpu (kept alive by self) and initialized; on an error it is
        // released again (without a `free` callback FFmpeg destroys
        // nothing of ours).
        unsafe {
            let mut buf = ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN);
            if buf.is_null() {
                return Err(CodecError::Backend("out of memory".into()));
            }
            let dc = (*buf).data.cast::<ffi::AVHWDeviceContext>();
            (*dc).user_opaque = Arc::as_ptr(&gpu).cast_mut().cast();
            let hw = &mut *(*dc).hwctx.cast::<ffi::AVVulkanDeviceContext>();
            hw.get_proc_addr = Some(std::mem::transmute::<
                vk::PFN_vkGetInstanceProcAddr,
                unsafe extern "C" fn(ffi::VkInstance, *const c_char) -> ffi::PFN_vkVoidFunction,
            >(gpu.entry.static_fn().get_instance_proc_addr));
            hw.inst = gpu.instance.handle().as_raw() as usize as ffi::VkInstance;
            hw.phys_dev = gpu.physical.as_raw() as usize as ffi::VkPhysicalDevice;
            hw.act_dev = gpu.device.handle().as_raw() as usize as ffi::VkDevice;
            ptr::write(
                ptr::addr_of_mut!(hw.device_features).cast::<vk::PhysicalDeviceFeatures2>(),
                features,
            );
            hw.enabled_inst_extensions = keep.instance_extensions.as_ptr();
            hw.nb_enabled_inst_extensions = keep.instance_extensions.len() as c_int;
            hw.enabled_dev_extensions = keep.device_extensions.as_ptr();
            hw.nb_enabled_dev_extensions = keep.device_extensions.len() as c_int;

            // One queue per family: the graphics one (also compute and
            // transfer) and the video ones.
            let graphics = gpu.queue_family as c_int;
            hw.queue_family_index = graphics;
            hw.nb_graphics_queues = 1;
            hw.queue_family_comp_index = graphics;
            hw.nb_comp_queues = 1;
            hw.queue_family_tx_index = graphics;
            hw.nb_tx_queues = 1;
            let (decode, nb_decode) = gpu
                .decode
                .as_ref()
                .map_or((-1, 0), |q| (q.family as c_int, 1));
            hw.queue_family_decode_index = decode;
            hw.nb_decode_queues = nb_decode;
            let (encode, nb_encode) = gpu
                .encode
                .as_ref()
                .map_or((-1, 0), |q| (q.family as c_int, 1));
            hw.queue_family_encode_index = encode;
            hw.nb_encode_queues = nb_encode;
            #[cfg(ffmpeg_queue_families)]
            list_queue_families(hw, &gpu);
            hw.lock_queue = Some(lock_queue);
            hw.unlock_queue = Some(unlock_queue);

            if let Err(e) = check(
                "set up FFmpeg's Vulkan device",
                ffi::av_hwdevice_ctx_init(buf),
            ) {
                ffi::av_buffer_unref(&mut buf);
                return Err(e);
            }
            Ok(Self {
                buf,
                gpu,
                _keep: keep,
            })
        }
    }

    pub fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
    }
}

impl Drop for VulkanDevice {
    fn drop(&mut self) {
        // SAFETY: our reference; whoever still holds one (a frames pool)
        // keeps the context alive, and the Gpu with it only as long as
        // self lives, so decoders drop their contexts first.
        unsafe { ffi::av_buffer_unref(&mut self.buf) };
    }
}

/// The queue families FFmpeg may use (FFmpeg 7.1 and later, which want
/// them listed besides the fixed fields): the graphics one, which also
/// computes and transfers, and the video ones.
#[cfg(ffmpeg_queue_families)]
fn list_queue_families(hw: &mut ffi::AVVulkanDeviceContext, gpu: &Gpu) {
    let graphics = vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER;
    let video = [
        (&gpu.decode, vk::QueueFlags::VIDEO_DECODE_KHR),
        (&gpu.encode, vk::QueueFlags::VIDEO_ENCODE_KHR),
    ];
    let families = std::iter::once((gpu.queue_family, graphics, 0)).chain(
        video
            .into_iter()
            .filter_map(|(q, flag)| q.as_ref().map(|q| (q.family, flag, q.operations.as_raw()))),
    );
    hw.nb_qf = 0;
    for (family, flags, ops) in families {
        hw.qf[hw.nb_qf as usize] = ffi::AVVulkanDeviceQueueFamily {
            idx: family as c_int,
            num: 1,
            flags: flags.as_raw(),
            video_caps: ops as c_int,
        };
        hw.nb_qf += 1;
    }
}

unsafe extern "C" fn pick_vulkan(
    _ctx: *mut ffi::AVCodecContext,
    mut fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    // SAFETY: FFmpeg passes a list terminated by AV_PIX_FMT_NONE.
    unsafe {
        while *fmts != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *fmts == ffi::AVPixelFormat::AV_PIX_FMT_VULKAN {
                return *fmts;
            }
            fmts = fmts.add(1);
        }
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

/// Names each frame pool a decoder uses, across decoders.
static POOLS: AtomicU64 = AtomicU64::new(1);

/// The last decoded frame, as [`VkSync`] for the presenter.
struct FrameSync {
    frame: *mut ffi::AVFrame,
}

impl FrameSync {
    /// The frame's pool and image.
    ///
    /// # Safety
    /// `frame` holds a decoded Vulkan frame.
    unsafe fn parts(&self) -> (*mut ffi::AVHWFramesContext, *mut ffi::AVVkFrame) {
        // SAFETY: as the caller promises.
        unsafe {
            let f = &*self.frame;
            (
                (*f.hw_frames_ctx).data.cast(),
                f.data[0].cast::<ffi::AVVkFrame>(),
            )
        }
    }
}

impl VkSync for FrameSync {
    fn begin(&self) -> VkAccess {
        // SAFETY: picture() hands this out only for a decoded Vulkan
        // frame, which stays until the next decode.
        unsafe {
            let (fc, vkf) = self.parts();
            let hw = &*(*fc).hwctx.cast::<ffi::AVVulkanFramesContext>();
            if let Some(lock) = hw.lock_frame {
                lock(fc, vkf);
            }
            let v = &*vkf;
            VkAccess {
                layout: v.layout[0],
                access: u64::from(v.access[0] as u32),
                semaphore: v.sem[0],
                wait: v.sem_value[0],
            }
        }
    }

    fn end(&self, submitted: bool, layout: i32, access: u64) {
        // SAFETY: as in begin(), which locked the frame.
        unsafe {
            let (fc, vkf) = self.parts();
            if submitted {
                let v = &mut *vkf;
                v.layout[0] = layout;
                v.access[0] = access as c_int;
                v.sem_value[0] += 1;
            }
            let hw = &*(*fc).hwctx.cast::<ffi::AVVulkanFramesContext>();
            if let Some(unlock) = hw.unlock_frame {
                unlock(fc, vkf);
            }
        }
    }
}

/// Decodes into Vulkan images on the presenter's device.
pub struct VulkanDecoder {
    ctx: *mut ffi::AVCodecContext,
    /// The last decoded frame (a Vulkan image).
    frame: *mut ffi::AVFrame,
    /// Receives from the decoder; moved into `frame` on success.
    recv: *mut ffi::AVFrame,
    sw: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    decoded: u64,
    /// The pool behind `frame` and its name ([`VkPicture::pool`]).
    pool: (*const c_void, u64),
    sync: FrameSync,
    nv12: Vec<u8>,
    // Last: the context above refers to it.
    device: VulkanDevice,
}

// SAFETY: as for VulkanDevice; the frames are only touched by the owner.
unsafe impl Send for VulkanDecoder {}

impl VulkanDecoder {
    /// A decoder for `codec` (H.264, HEVC or AV1) on `gpu`, the device the
    /// pictures are drawn with.
    pub fn for_codec(gpu: Arc<Gpu>, codec: Codec) -> Result<Self, CodecError> {
        if !ffmpeg_is_ready() {
            return Err(CodecError::Backend(
                "FFmpeg before 7.1: no Vulkan Video decoding".into(),
            ));
        }
        if !decodes(&gpu, codec) {
            return Err(CodecError::Backend(format!(
                "{} has no Vulkan Video decoder for {codec:?}",
                gpu.name()
            )));
        }
        let names = ff::names(codec, ff::Backend::Vulkan)?;
        let device = VulkanDevice::new(gpu)?;
        // SAFETY: everything allocated here is owned by the decoder and
        // freed in Drop (null pointers included).
        unsafe {
            let decoder = ffi::avcodec_find_decoder_by_name(names.decoder.as_ptr());
            if decoder.is_null() {
                return Err(CodecError::Backend(format!(
                    "FFmpeg has no {} decoder",
                    names.label
                )));
            }
            let frame = ffi::av_frame_alloc();
            let dec = Self {
                ctx: ffi::avcodec_alloc_context3(decoder),
                frame,
                recv: ffi::av_frame_alloc(),
                sw: ffi::av_frame_alloc(),
                pkt: ffi::av_packet_alloc(),
                decoded: 0,
                pool: (ptr::null(), 0),
                sync: FrameSync { frame },
                nv12: Vec::new(),
                device,
            };
            if dec.ctx.is_null()
                || dec.frame.is_null()
                || dec.recv.is_null()
                || dec.sw.is_null()
                || dec.pkt.is_null()
            {
                return Err(CodecError::Backend("out of memory".into()));
            }
            (*dec.ctx).hw_device_ctx = ffi::av_buffer_ref(dec.device.buf);
            (*dec.ctx).get_format = Some(pick_vulkan);
            (*dec.ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as c_int;
            // Frame threading would delay output by a frame per thread.
            (*dec.ctx).thread_count = 1;
            check(
                "open the decoder",
                ffi::avcodec_open2(dec.ctx, decoder, ptr::null_mut()),
            )?;
            Ok(dec)
        }
    }

    /// Waits until the GPU has decoded the last frame. FFmpeg only queues
    /// the work; waiting here keeps the decode time where it belongs (the
    /// overlay's Decode, not the display) and matches the other decoders.
    fn wait_decoded(&self) -> Result<(), CodecError> {
        let access = self.sync.begin();
        self.sync.end(false, access.layout, access.access);
        let semaphores = [vk::Semaphore::from_raw(access.semaphore)];
        let values = [access.wait];
        // SAFETY: the frame's semaphore, on our device.
        unsafe {
            self.device.gpu.device.wait_semaphores(
                &vk::SemaphoreWaitInfo::default()
                    .semaphores(&semaphores)
                    .values(&values),
                fernsicht_gpu::GPU_TIMEOUT.as_nanos() as u64,
            )
        }
        .map_err(|e| CodecError::Backend(format!("wait for the decoder: {e}")))
    }

    /// Whether the stream says its last frame is BT.709 limited range.
    pub fn last_frame_is_bt709_limited(&self) -> bool {
        // SAFETY: frame is a valid (possibly empty) AVFrame.
        unsafe { is_bt709_limited(self.frame) }
    }

    fn download(&mut self) -> Result<(), CodecError> {
        // SAFETY: frame holds the last decoded Vulkan frame (or nothing,
        // in which case the transfer fails cleanly).
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

impl Decoder for VulkanDecoder {
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
                let pool = if f.hw_frames_ctx.is_null() {
                    ptr::null()
                } else {
                    (*f.hw_frames_ctx).data.cast_const().cast()
                };
                if pool != self.pool.0 {
                    self.pool = (pool, POOLS.fetch_add(1, Ordering::Relaxed));
                }
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
        self.wait_decoded()
    }

    /// The decoded image itself where the presenter wants a GPU picture,
    /// else NV12 downloaded to CPU memory.
    fn picture(&mut self, prefer: PictureKind) -> Result<Option<Picture<'_>>, CodecError> {
        // SAFETY: frame is a valid AVFrame; a Vulkan one carries its pool
        // and an AVVkFrame.
        let (width, height, image, coded) = unsafe {
            let f = &*self.frame;
            if f.format != ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as c_int {
                return Ok(None);
            }
            let fc = &*(*f.hw_frames_ctx).data.cast::<ffi::AVHWFramesContext>();
            let vkf = &*f.data[0].cast::<ffi::AVVkFrame>();
            if vkf.img[1] != 0 {
                // One image per plane: the presenter takes multi-planar
                // images only.
                return Err(CodecError::Backend(
                    "Vulkan decoder made one image per plane".into(),
                ));
            }
            (
                f.width as u32,
                f.height as u32,
                vkf.img[0],
                (fc.width as u32, fc.height as u32),
            )
        };
        if prefer == PictureKind::Gpu {
            return Ok(Some(Picture::Vulkan(VkPicture {
                image,
                coded,
                width,
                height,
                pool: self.pool.1,
                sync: &self.sync,
            })));
        }
        self.download()?;
        Ok(Some(Picture::Nv12 {
            width,
            height,
            data: &self.nv12,
        }))
    }
}

impl Drop for VulkanDecoder {
    fn drop(&mut self) {
        // The presenter may still read the last picture.
        self.device.gpu.wait_idle();
        // SAFETY: each pointer is either null or owned by us; the device
        // context outlives them (dropped after this).
        unsafe {
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_frame_free(&mut self.frame);
            ffi::av_frame_free(&mut self.recv);
            ffi::av_frame_free(&mut self.sw);
            ffi::av_packet_free(&mut self.pkt);
        }
    }
}
