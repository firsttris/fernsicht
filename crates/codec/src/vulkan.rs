//! H.264, HEVC and AV1 with Vulkan Video, via FFmpeg, on our own Vulkan
//! device.
//!
//! FFmpeg's Vulkan decoder runs on the device the presenter draws with
//! ([`fernsicht_gpu::Gpu`]): it is handed our instance, device and queues
//! (sharing their locks), decodes into its pool of NV12 images, and the
//! presenter samples the decoded image in place: no copy, no second API.
//! Works wherever the driver has Vulkan Video decode (NVIDIA from Pascal,
//! AMD and Intel with Mesa).
//!
//! The encoder takes the screen the same way: our converter turns the RGB
//! DMA-BUF into NV12 and, in the same submission, copies it into an image
//! of FFmpeg's encoder pool, which FFmpeg's Vulkan encoder reads.
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
use fernsicht_capture::{DmaBuf, Frame, PixelFormat};
use fernsicht_core::colour::{Colour, Depth};
use fernsicht_core::now_us;
use fernsicht_gpu::convert::Converter;
use fernsicht_gpu::{Gpu, Prefer, Timeline};
use fernsicht_proto::Codec;
use ffmpeg_next::ffi;

use crate::ff::{
    self, check, eagain, ff_err, is_bt709_limited, nv12_from_frame, nv12_into_frame, signal_colour,
    yuv_format,
};
use crate::{
    CodecError, DecodedFrame, Decoder, EncodedFrame, Encoder, Picture, PictureKind, VkAccess,
    VkPicture, VkSync,
};

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

/// [`decodes`] at `depth`: at 10 bit the driver's capabilities for the
/// profile (HEVC Main 10, AV1 Main at 10 bit) decide; a GPU that decodes
/// a codec need not decode it deeper.
pub fn decodes_in(gpu: &Gpu, codec: Codec, depth: Depth) -> bool {
    decodes(gpu, codec) && (depth == Depth::Eight || decodes_ten_bit(gpu, codec))
}

/// Whether the driver has a 10-bit 4:2:0 decode profile for `codec`
/// (vkGetPhysicalDeviceVideoCapabilitiesKHR: success, not
/// VK_ERROR_VIDEO_PROFILE_*_NOT_SUPPORTED).
fn decodes_ten_bit(gpu: &Gpu, codec: Codec) -> bool {
    use ash::vk::native;
    let video = ash::khr::video_queue::Instance::new(&gpu.entry, &gpu.instance);
    let profile = vk::VideoProfileInfoKHR::default()
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_10)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_10);
    let mut decode = vk::VideoDecodeCapabilitiesKHR::default();
    let caps = vk::VideoCapabilitiesKHR::default().push_next(&mut decode);
    let query = |profile: &vk::VideoProfileInfoKHR, caps: &mut vk::VideoCapabilitiesKHR| {
        // SAFETY: valid physical device; the structs are chained as the
        // profile's codec asks.
        unsafe {
            (video.fp().get_physical_device_video_capabilities_khr)(gpu.physical, profile, caps)
                == vk::Result::SUCCESS
        }
    };
    match codec {
        Codec::Hevc => {
            let mut h265 = vk::VideoDecodeH265ProfileInfoKHR::default()
                .std_profile_idc(native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10);
            let mut h265_caps = vk::VideoDecodeH265CapabilitiesKHR::default();
            query(
                &profile
                    .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_H265)
                    .push_next(&mut h265),
                &mut caps.push_next(&mut h265_caps),
            )
        }
        Codec::Av1 => {
            let mut av1 = vk::VideoDecodeAV1ProfileInfoKHR::default()
                .std_profile(native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN);
            let mut av1_caps = vk::VideoDecodeAV1CapabilitiesKHR::default();
            query(
                &profile
                    .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_AV1)
                    .push_next(&mut av1),
                &mut caps.push_next(&mut av1_caps),
            )
        }
        Codec::H264 | Codec::Synthetic => false,
    }
}

/// Whether `gpu` encodes `codec` with Vulkan Video, as far as the driver
/// says (an encode queue with the codec) and the FFmpeg loaded is
/// [ready](ffmpeg_is_ready). H.264 and HEVC; not AV1 (see below).
pub fn gpu_encodes(gpu: &Gpu, codec: Codec) -> bool {
    let op = match codec {
        Codec::H264 => vk::VideoCodecOperationFlagsKHR::ENCODE_H264,
        Codec::Hevc => vk::VideoCodecOperationFlagsKHR::ENCODE_H265,
        // Not offered: FFmpeg 8.1's Vulkan AV1 encoder writes the
        // encoder's aligned surface size as the frame size
        // (vulkan_encode_av1.c: frame and render size = surface_width/
        // height, 1920×1088 on RADV for 1080p), and AV1 has no cropping:
        // the picture gets 8 extra rows. The host takes HEVC instead.
        Codec::Av1 | Codec::Synthetic => return false,
    };
    ffmpeg_is_ready()
        && gpu
            .encode
            .as_ref()
            .is_some_and(|q| q.operations.contains(op))
}

/// Whether the GPU behind `render_node` encodes `codec` with Vulkan Video:
/// an encoder for a small picture opens.
pub fn encodes(render_node: &str, codec: Codec) -> bool {
    encodes_in(render_node, codec, Depth::Eight)
}

/// [`encodes`] at `depth` (10 bit: HEVC Main 10).
pub fn encodes_in(render_node: &str, codec: Codec, depth: Depth) -> bool {
    codec != Codec::Synthetic
        && VulkanEncoder::new(&VulkanEncoderConfig {
            render_node: render_node.into(),
            codec,
            width: 256,
            height: 256,
            fps: 30,
            bitrate_kbps: 1_000,
            colour: Colour::SDR.with_depth(depth),
        })
        .is_ok()
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

/// How long a decoded frame may take before the decoder counts as hung.
/// Longer than the GPU's usual limit: the first frame of a new session
/// took just over a second once on the NVIDIA runner (GTX 1080).
const DECODE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Fails once `gpu` was given up: FFmpeg would wait for it for ever.
fn usable(gpu: &Gpu) -> Result<(), CodecError> {
    if gpu.is_lost() {
        Err(CodecError::GpuLost)
    } else {
        Ok(())
    }
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
        let names = ff::names(codec, ff::Backend::Vulkan, Depth::Eight)?;
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
                DECODE_TIMEOUT.as_nanos() as u64,
            )
        }
        .map_err(|e| {
            if matches!(e, vk::Result::TIMEOUT | vk::Result::ERROR_DEVICE_LOST) {
                // FFmpeg would wait for this frame for ever when it frees it.
                self.device.gpu.give_up(&format!("the decoder: {e}"));
                CodecError::GpuLost
            } else {
                CodecError::Backend(format!("wait for the decoder: {e}"))
            }
        })
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
            (*self.sw).format = ff::sw_format(self.frame) as c_int;
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
        usable(&self.device.gpu)?;
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
        let (width, height, image, coded, colour) = unsafe {
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
                ff::colour_of(self.frame, fc.sw_format),
            )
        };
        if prefer == PictureKind::Gpu {
            return Ok(Some(Picture::Vulkan(VkPicture {
                image,
                coded,
                width,
                height,
                pool: self.pool.1,
                colour,
                sync: &self.sync,
            })));
        }
        self.download()?;
        Ok(Some(Picture::Nv12 {
            width,
            height,
            data: &self.nv12,
            colour,
        }))
    }
}

impl Drop for VulkanDecoder {
    fn drop(&mut self) {
        if self.device.gpu.is_lost() {
            // FFmpeg would wait for the GPU for ever while freeing its
            // frames (ff_vk_decode_free_frame): left to the process's end.
            return;
        }
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

/// Frames between two IDR frames unless one is asked for: "endless" as
/// far as HEVC allows. FFmpeg sizes the picture order count to the GOP
/// (`vulkan_encode_h265.c`, `log2_max_pic_order_cnt_lsb_minus4`), which
/// has at most 16 bits; 2^15 frames is an IDR frame every 9 minutes at
/// 60 fps.
const GOP: c_int = 1 << 15;

/// An encoder tuning (FFmpeg's `tune` option); `None` is the driver's
/// default.
type Tune = Option<&'static std::ffi::CStr>;

/// Tunings to try, lowest latency first.
const TUNES: [Tune; 3] = [Some(c"ull"), Some(c"ll"), None];

/// Encoder settings.
#[derive(Clone, Debug)]
pub struct VulkanEncoderConfig {
    /// The GPU's DRM render node: the Vulkan device is the one behind it,
    /// where the screen's DMA-BUFs import.
    pub render_node: String,
    /// H.264, HEVC or AV1.
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// The stream's colour: 10 bit takes the screen (DMA-BUF) only, CPU
    /// input is 8-bit NV12.
    pub colour: Colour,
}

/// Encodes with Vulkan Video. Settings as for the other hardware encoders:
/// no B-frames, IDR frames on request (else only every [`GOP`] frames),
/// CBR with a buffer of one frame, one frame in flight (`async_depth` 1),
/// tuned for the lowest latency the driver offers and desktop content.
pub struct VulkanEncoder {
    ctx: *mut ffi::AVCodecContext,
    frames: *mut ffi::AVBufferRef,
    /// The frame being encoded (an image of `frames`).
    hw: *mut ffi::AVFrame,
    /// CPU NV12 pictures go through it.
    sw: *mut ffi::AVFrame,
    pkt: *mut ffi::AVPacket,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    colour: Colour,
    /// Made on the first DMA-BUF frame.
    converter: Option<Converter>,
    force_keyframe: bool,
    next_pts: i64,
    /// The tuning the driver took (of [`TUNES`]), once known.
    tune: Option<Tune>,
    // Last: the contexts above refer to it.
    device: VulkanDevice,
}

// SAFETY: all FFmpeg and Vulkan objects are owned by this value and only
// touched through &mut self.
unsafe impl Send for VulkanEncoder {}

impl VulkanEncoder {
    pub fn new(cfg: &VulkanEncoderConfig) -> Result<Self, CodecError> {
        if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) || cfg.fps == 0 {
            return Err(CodecError::Backend(
                "width/height must be even, fps > 0".into(),
            ));
        }
        let backend = |e: String| CodecError::Backend(e);
        let gpu = Gpu::preferring(Prefer::render_node(&cfg.render_node)).map_err(backend)?;
        if !gpu_encodes(&gpu, cfg.codec) {
            return Err(CodecError::Backend(format!(
                "{} has no Vulkan Video encoder for {:?} (or FFmpeg is older than 7.1)",
                gpu.name(),
                cfg.codec
            )));
        }
        let device = VulkanDevice::new(Arc::new(gpu))?;
        // SAFETY: every pointer is checked before use and released in Drop;
        // the struct is built first, so Drop sees nulls for anything not
        // yet created.
        unsafe {
            let mut enc = Self {
                ctx: ptr::null_mut(),
                frames: ptr::null_mut(),
                hw: ffi::av_frame_alloc(),
                sw: ffi::av_frame_alloc(),
                pkt: ffi::av_packet_alloc(),
                codec: cfg.codec,
                width: cfg.width,
                height: cfg.height,
                fps: cfg.fps,
                colour: cfg.colour,
                converter: None,
                force_keyframe: true,
                next_pts: 0,
                tune: None,
                device,
            };
            if enc.hw.is_null() || enc.sw.is_null() || enc.pkt.is_null() {
                return Err(CodecError::Backend("out of memory".into()));
            }
            enc.frames = ffi::av_hwframe_ctx_alloc(enc.device.buf);
            if enc.frames.is_null() {
                return Err(CodecError::Backend("out of memory".into()));
            }
            let fc = (*enc.frames).data.cast::<ffi::AVHWFramesContext>();
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN;
            (*fc).sw_format = yuv_format(cfg.colour.depth);
            (*fc).width = cfg.width as c_int;
            (*fc).height = cfg.height as c_int;
            // One multi-planar image per frame: the copy below writes both
            // planes of it.
            (*(*fc).hwctx.cast::<ffi::AVVulkanFramesContext>()).flags =
                ffi::AVVkFrameFlags::AV_VK_FRAME_FLAG_NONE;
            check(
                "set up the encoder's frame pool",
                ffi::av_hwframe_ctx_init(enc.frames),
            )?;
            enc.ctx = enc.open_codec(cfg.bitrate_kbps)?;

            (*enc.sw).format = yuv_format(cfg.colour.depth) as c_int;
            (*enc.sw).width = cfg.width as c_int;
            (*enc.sw).height = cfg.height as c_int;
            check("allocate upload frame", ffi::av_frame_get_buffer(enc.sw, 0))?;
            Ok(enc)
        }
    }

    /// The codec context: CBR at `kbps`, on this encoder's frame pool.
    ///
    /// # Safety
    /// `self.frames` is an initialised frame pool.
    unsafe fn open_codec(&mut self, kbps: u32) -> Result<*mut ffi::AVCodecContext, CodecError> {
        // The lowest-latency tuning the driver takes, found once: NVIDIA's
        // (GTX 1080) has no ultra-low-latency profile and refuses the
        // whole profile.
        let tunes = match self.tune {
            Some(known) => vec![known],
            None => TUNES.to_vec(),
        };
        let mut tried = Vec::new();
        for tune in tunes {
            // SAFETY: passed on.
            match unsafe { self.open_tuned(kbps, tune) } {
                Ok(ctx) => {
                    self.tune = Some(tune);
                    return Ok(ctx);
                }
                Err(e) => tried.push(e.to_string()),
            }
        }
        Err(CodecError::Backend(tried.join("; ")))
    }

    /// [`Self::open_codec`] with one tuning (`None`: the driver's default).
    ///
    /// # Safety
    /// As for [`Self::open_codec`].
    unsafe fn open_tuned(
        &self,
        kbps: u32,
        tune: Tune,
    ) -> Result<*mut ffi::AVCodecContext, CodecError> {
        let names = ff::names(self.codec, ff::Backend::Vulkan, self.colour.depth)?;
        let mut opts = vec![
            (c"async_depth", c"1"),
            (c"rc_mode", c"cbr"),
            (c"content", c"desktop"),
        ];
        opts.extend(tune.map(|t| (c"tune", t)));
        if let Some(profile) = names.profile {
            opts.push((c"profile", profile));
        }
        let (width, height, fps, frames) = (self.width, self.height, self.fps, self.frames);
        let colour = self.colour;
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
                (*c).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN;
                // The video profile (chroma subsampling, bit depth) comes
                // from it (vulkan_encode.c, init_profile).
                (*c).sw_pix_fmt = yuv_format(colour.depth);
                (*c).hw_frames_ctx = ffi::av_buffer_ref(frames);
                (*c).max_b_frames = 0;
                (*c).gop_size = GOP;
                (*c).keyint_min = GOP;
                ff::set_cbr(c, kbps, fps);
                (*c).flags |=
                    (ffi::AV_CODEC_FLAG_LOW_DELAY | ffi::AV_CODEC_FLAG_CLOSED_GOP) as c_int;
                (*c).thread_count = 1;
                // The way the converter makes YCbCr, written into the
                // stream.
                signal_colour(c, colour);
            })
        }
    }

    /// CPU NV12 → `self.hw`.
    ///
    /// # Safety
    /// `self.hw` is an empty frame.
    unsafe fn upload(&mut self, frame: &Frame) -> Result<(), CodecError> {
        if self.colour.depth != Depth::Eight {
            return Err(CodecError::Backend(
                "a 10-bit stream takes the screen (DMA-BUF), not CPU pictures".into(),
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
        // SAFETY: as the caller promises; sw was allocated for this size.
        unsafe {
            check(
                "get an encoder image",
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
                "upload to the GPU",
                ffi::av_hwframe_transfer_data(self.hw, self.sw, 0),
            )?;
        }
        Ok(())
    }

    /// The screen → NV12 → an encoder image, in one submission on the GPU.
    ///
    /// # Safety
    /// `self.hw` is an empty frame.
    unsafe fn convert_dmabuf(&mut self, image: &DmaBuf) -> Result<(), CodecError> {
        let backend = |e: String| CodecError::Backend(e);
        if self.converter.is_none() {
            let gpu = self.device.gpu.clone();
            let name = gpu.name().to_string();
            self.converter = Some(
                Converter::on_device(gpu, self.width, self.height, self.colour).map_err(backend)?,
            );
            log::info!("Vulkan Video takes the screen on {name} without leaving the GPU");
        }
        let converter = self.converter.as_mut().expect("made above");
        let layout = converter.layout();
        // SAFETY: as the caller promises; the frame is a multi-planar image
        // of our pool (flags NONE), locked while its state is read and
        // written around the submission.
        unsafe {
            check(
                "get an encoder image",
                ffi::av_hwframe_get_buffer(self.frames, self.hw, 0),
            )?;
            let sync = FrameSync { frame: self.hw };
            let (_, vkf) = sync.parts();
            if (*vkf).img[1] != 0 {
                return Err(CodecError::Backend(
                    "the encoder's images have one image per plane".into(),
                ));
            }
            let target = vk::Image::from_raw((*vkf).img[0]);
            let access = sync.begin();
            let timeline = Timeline {
                semaphore: vk::Semaphore::from_raw(access.semaphore),
                wait: access.wait,
                signal: access.wait + 1,
            };
            let (w, h) = (layout.width, layout.height);
            let result = converter.convert_with(
                image,
                |d, cmd, buffer| {
                    let range = vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1);
                    let to_dst = vk::ImageMemoryBarrier::default()
                        .image(target)
                        .old_layout(vk::ImageLayout::from_raw(access.layout))
                        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .src_access_mask(vk::AccessFlags::from_raw(access.access as u32))
                        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .subresource_range(range);
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::ALL_COMMANDS,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[to_dst],
                    );
                    let plane = |aspect, offset, row_texels, width, height| {
                        vk::BufferImageCopy::default()
                            .buffer_offset(offset)
                            .buffer_row_length(row_texels)
                            .image_subresource(
                                vk::ImageSubresourceLayers::default()
                                    .aspect_mask(aspect)
                                    .layer_count(1),
                            )
                            .image_extent(vk::Extent3D {
                                width,
                                height,
                                depth: 1,
                            })
                    };
                    // Row lengths count texels: one sample for Y, two for
                    // UV (a sample is a byte, or a 16-bit word at 10 bit).
                    let texel = layout.depth.sample_bytes();
                    let copies = [
                        plane(vk::ImageAspectFlags::PLANE_0, 0, layout.pitch / texel, w, h),
                        plane(
                            vk::ImageAspectFlags::PLANE_1,
                            layout.uv_offset,
                            layout.pitch / (2 * texel),
                            w / 2,
                            h / 2,
                        ),
                    ];
                    d.cmd_copy_buffer_to_image(
                        cmd,
                        buffer,
                        target,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &copies,
                    );
                },
                Some(timeline),
            );
            sync.end(
                result.is_ok(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL.as_raw(),
                u64::from(vk::AccessFlags::TRANSFER_WRITE.as_raw()),
            );
            result.map_err(|e| {
                if self.device.gpu.is_lost() {
                    CodecError::GpuLost
                } else {
                    CodecError::Backend(e)
                }
            })
        }
    }

    /// Screen buffers whose import is kept (see `DmaBuf::identity`).
    pub fn cached_imports(&self) -> usize {
        self.converter.as_ref().map_or(0, Converter::cached_imports)
    }
}

impl Encoder for VulkanEncoder {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn colour(&self) -> Colour {
        self.colour
    }

    fn encode(&mut self, frame: &Frame, out: &mut EncodedFrame) -> Result<(), CodecError> {
        out.data.clear();
        out.keyframe = false;
        usable(&self.device.gpu)?;
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
                // Makes FFmpeg's hardware encoders emit an IDR frame.
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

    fn set_bitrate(&mut self, kbps: u32) -> Result<(), CodecError> {
        // FFmpeg sets the rate control when the codec opens: a new codec
        // context on the same device and frame pool. It starts with a
        // keyframe.
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
}

impl Drop for VulkanEncoder {
    fn drop(&mut self) {
        if self.device.gpu.is_lost() {
            // As for the decoder: FFmpeg would wait for the GPU for ever.
            return;
        }
        self.converter = None;
        self.device.gpu.wait_idle();
        // SAFETY: each pointer is either null or owned by us; the device
        // context outlives them (dropped after this).
        unsafe {
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_frame_free(&mut self.hw);
            ffi::av_frame_free(&mut self.sw);
            ffi::av_packet_free(&mut self.pkt);
            ffi::av_buffer_unref(&mut self.frames);
        }
    }
}
