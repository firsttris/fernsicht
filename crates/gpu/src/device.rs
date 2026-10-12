//! Vulkan instance, device and queue.

use std::ffi::{CStr, c_char};

use ash::vk;

/// Device extensions needed to sample DMA-BUFs without a copy.
const DMABUF_EXTENSIONS: [&CStr; 4] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];

/// Device extensions for video work on the GPU (FFmpeg's Vulkan codecs),
/// each enabled where the driver has it: the queues, then the codecs.
const VIDEO_EXTENSIONS: [&CStr; 3] = [
    c"VK_KHR_video_queue",
    c"VK_KHR_video_maintenance1",
    c"VK_KHR_video_maintenance2",
];
const DECODE_EXTENSIONS: [&CStr; 4] = [
    c"VK_KHR_video_decode_queue",
    c"VK_KHR_video_decode_h264",
    c"VK_KHR_video_decode_h265",
    c"VK_KHR_video_decode_av1",
];
const ENCODE_EXTENSIONS: [&CStr; 4] = [
    c"VK_KHR_video_encode_queue",
    c"VK_KHR_video_encode_h264",
    c"VK_KHR_video_encode_h265",
    c"VK_KHR_video_encode_av1",
];

/// How often device creation is tried before giving up.
const DEVICE_ATTEMPTS: u64 = 4;

/// Errors where another attempt can succeed; a missing extension or
/// feature will not appear by waiting.
fn device_error_is_transient(e: vk::Result) -> bool {
    !matches!(
        e,
        vk::Result::ERROR_EXTENSION_NOT_PRESENT | vk::Result::ERROR_FEATURE_NOT_PRESENT
    )
}

/// Picks the device whose name contains this (case-insensitive), e.g.
/// `llvmpipe` for the software rasterizer.
pub const DEVICE_ENV: &str = "FERNSICHT_VULKAN_DEVICE";

/// A queue's lock: Vulkan requires submissions to one queue to be
/// serialized. Unlike a mutex guard it can also be taken and given back
/// by separate calls, as FFmpeg's `lock_queue`/`unlock_queue` do.
#[derive(Default)]
pub struct QueueLock {
    held: std::sync::Mutex<bool>,
    freed: std::sync::Condvar,
}

/// Holds a [`QueueLock`] until dropped.
pub struct QueueGuard<'a>(&'a QueueLock);

impl QueueLock {
    pub fn lock(&self) -> QueueGuard<'_> {
        self.acquire();
        QueueGuard(self)
    }

    /// Takes the lock; [`Self::release`] gives it back.
    pub fn acquire(&self) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        while *held {
            held = self.freed.wait(held).unwrap_or_else(|e| e.into_inner());
        }
        *held = true;
    }

    /// Gives back what [`Self::acquire`] took.
    pub fn release(&self) {
        *self.held.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.freed.notify_one();
    }
}

impl Drop for QueueGuard<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A queue family for video work: its index, the codec operations it
/// does, and the lock of its first queue (the one created).
pub struct VideoQueue {
    pub family: u32,
    pub operations: vk::VideoCodecOperationFlagsKHR,
    pub lock: QueueLock,
}

/// Features enabled beyond Vulkan 1.0 (FFmpeg has to be told).
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    pub sampler_ycbcr_conversion: bool,
    pub timeline_semaphore: bool,
    pub synchronization2: bool,
}

pub struct Gpu {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    /// Several renderers, and FFmpeg, may share the queue.
    pub queue_lock: QueueLock,
    pub queue_family: u32,
    /// Video decode and encode queues, where the GPU has them.
    pub decode: Option<VideoQueue>,
    pub encode: Option<VideoQueue>,
    /// What the instance and device were created with.
    pub instance_extensions: Vec<std::ffi::CString>,
    pub device_extensions: Vec<&'static CStr>,
    pub features: Features,
    /// The GPU stopped finishing work ([`Gpu::give_up`]).
    lost: std::sync::atomic::AtomicBool,
    memory: vk::PhysicalDeviceMemoryProperties,
    /// Present when the DMA-BUF import extensions are enabled.
    pub memory_fd: Option<ash::khr::external_memory_fd::Device>,
    /// Present when created for a window.
    pub swapchain: Option<ash::khr::swapchain::Device>,
    name: String,
    uuid: [u8; 16],
}

/// Which GPU to take when there are several.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Prefer {
    /// Discrete before integrated.
    #[default]
    Best,
    /// The one with this DRM render node (major, minor), e.g. the VAAPI
    /// decoder's: its DMA-BUFs import only on the same GPU.
    RenderNode(u32, u32),
    /// The one with this device UUID, e.g. the CUDA device NVENC runs on.
    Uuid([u8; 16]),
}

impl Prefer {
    /// The GPU behind a render node such as `/dev/dri/renderD128`; `Best`
    /// if there is no such node.
    pub fn render_node(path: &str) -> Self {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).map_or(Prefer::Best, |m| {
            let (major, minor) = dev_numbers(m.rdev());
            Prefer::RenderNode(major, minor)
        })
    }

    fn matches(&self, c: &Candidate) -> bool {
        match *self {
            Prefer::Best => true,
            Prefer::RenderNode(major, minor) => c.render == Some((major, minor)),
            Prefer::Uuid(uuid) => c.uuid == uuid,
        }
    }
}

/// Major and minor of a device number (Linux's encoding, as glibc's
/// `major()`/`minor()`).
fn dev_numbers(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

/// What decides between Vulkan devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Candidate {
    /// [`rank`] of its type.
    rank: u32,
    /// Its render node, where the driver says (`VK_EXT_physical_device_drm`).
    render: Option<(u32, u32)>,
    uuid: [u8; 16],
}

/// The device to take: the preferred one if there is one, else the best
/// kind; among equals the first.
fn pick(candidates: &[Candidate], prefer: &Prefer) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .min_by_key(|(i, c)| (!prefer.matches(c), c.rank, *i))
        .map(|(i, _)| i)
}

/// A Vulkan error with what was being done.
pub(crate) fn err(what: &'static str) -> impl Fn(vk::Result) -> String {
    move |e| format!("{what}: {e}")
}

/// How long a submission may take before the GPU counts as hung.
pub const GPU_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Submits `cmds` (waiting on `wait` at colour output, signalling
/// `signal`) and waits for `fence` ([`wait_fence`]).
///
/// # Safety
/// As for [`submit`].
pub unsafe fn submit_with(
    gpu: &Gpu,
    cmds: &[vk::CommandBuffer],
    wait: Option<vk::Semaphore>,
    signal: Option<vk::Semaphore>,
    fence: vk::Fence,
) -> Result<(), String> {
    // SAFETY: passed on.
    unsafe {
        submit(gpu, cmds, wait, signal, fence)?;
        wait_fence(gpu, fence)
    }
}

/// Submits `cmds` (waiting on `wait` at colour output, signalling
/// `signal`, then `fence`) without waiting: [`wait_fence`] before the
/// fence or anything the commands use is touched again.
///
/// # Safety
/// The command buffers, semaphores and fence belong to `gpu`'s device and
/// the fence is unsignalled.
pub unsafe fn submit(
    gpu: &Gpu,
    cmds: &[vk::CommandBuffer],
    wait: Option<vk::Semaphore>,
    signal: Option<vk::Semaphore>,
    fence: vk::Fence,
) -> Result<(), String> {
    // SAFETY: passed on.
    unsafe { submit_timeline(gpu, cmds, wait, signal, None, fence) }
}

/// A timeline semaphore a submission waits for (before anything) and
/// signals.
#[derive(Clone, Copy, Debug)]
pub struct Timeline {
    pub semaphore: vk::Semaphore,
    pub wait: u64,
    pub signal: u64,
}

/// [`submit`], also waiting for and signalling `timeline`.
///
/// # Safety
/// As for [`submit`]; the timeline semaphore belongs to the device too.
pub unsafe fn submit_timeline(
    gpu: &Gpu,
    cmds: &[vk::CommandBuffer],
    wait: Option<vk::Semaphore>,
    signal: Option<vk::Semaphore>,
    timeline: Option<Timeline>,
    fence: vk::Fence,
) -> Result<(), String> {
    let mut waits: Vec<vk::Semaphore> = wait.into_iter().collect();
    let mut stages = vec![vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT; waits.len()];
    let mut signals: Vec<vk::Semaphore> = signal.into_iter().collect();
    // Values for binary semaphores are ignored.
    let mut wait_values = vec![0; waits.len()];
    let mut signal_values = vec![0; signals.len()];
    if let Some(t) = timeline {
        waits.push(t.semaphore);
        stages.push(vk::PipelineStageFlags::ALL_COMMANDS);
        wait_values.push(t.wait);
        signals.push(t.semaphore);
        signal_values.push(t.signal);
    }
    let mut values = vk::TimelineSemaphoreSubmitInfo::default()
        .wait_semaphore_values(&wait_values)
        .signal_semaphore_values(&signal_values);
    let mut submit = vk::SubmitInfo::default()
        .command_buffers(cmds)
        .wait_semaphores(&waits)
        .wait_dst_stage_mask(&stages)
        .signal_semaphores(&signals);
    if timeline.is_some() {
        submit = submit.push_next(&mut values);
    }
    if gpu.is_lost() {
        return Err(GIVEN_UP.into());
    }
    // SAFETY: as the caller promises; submissions are serialized by the
    // queue lock.
    let submitted = unsafe {
        let _queue = gpu.queue_lock.lock();
        gpu.device.queue_submit(gpu.queue, &[submit], fence)
    };
    match submitted {
        Ok(()) => Ok(()),
        Err(vk::Result::ERROR_DEVICE_LOST) => {
            gpu.give_up("device lost");
            Err(GIVEN_UP.into())
        }
        Err(e) => Err(format!("submit: {e}")),
    }
}

/// What submissions to a GPU that was given up fail with.
pub const GIVEN_UP: &str = "the GPU stopped responding and was given up";

/// Waits for a submission's `fence`, at most [`GPU_TIMEOUT`], and
/// unsignals it again for the next submission
/// (VUID-vkQueueSubmit-fence-00063). Work that does not finish in time
/// gives the GPU up ([`Gpu::give_up`]): the fence stays pending, and
/// nothing waits for it.
///
/// # Safety
/// `fence` belongs to `gpu`'s device and was submitted.
pub unsafe fn wait_fence(gpu: &Gpu, fence: vk::Fence) -> Result<(), String> {
    let d = &gpu.device;
    // SAFETY: as the caller promises.
    unsafe {
        match d.wait_for_fences(&[fence], true, GPU_TIMEOUT.as_nanos() as u64) {
            Ok(()) => d
                .reset_fences(&[fence])
                .map_err(|e| format!("reset fence: {e}")),
            Err(vk::Result::TIMEOUT) => {
                gpu.give_up(&format!("work did not finish within {GPU_TIMEOUT:?}"));
                Err(format!(
                    "the GPU did not finish within {GPU_TIMEOUT:?} (hung?); {GIVEN_UP}"
                ))
            }
            Err(vk::Result::ERROR_DEVICE_LOST) => {
                gpu.give_up("device lost");
                Err(GIVEN_UP.into())
            }
            Err(e) => Err(format!("wait for the GPU: {e}")),
        }
    }
}

/// [`submit_with`] without semaphores.
///
/// # Safety
/// As for [`submit_with`].
pub unsafe fn submit_and_wait(
    gpu: &Gpu,
    cmds: &[vk::CommandBuffer],
    fence: vk::Fence,
) -> Result<(), String> {
    // SAFETY: passed on.
    unsafe { submit_with(gpu, cmds, None, None, fence) }
}

/// The queue families for video decode and encode with the codec
/// operations each does; the first of each kind.
type VideoFamily = (u32, vk::VideoCodecOperationFlagsKHR);

/// # Safety
/// `physical` belongs to `instance`.
unsafe fn video_families(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> (Option<VideoFamily>, Option<VideoFamily>) {
    // SAFETY: as the caller promises; the property structs outlive the
    // query.
    unsafe {
        let n = instance.get_physical_device_queue_family_properties2_len(physical);
        let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
        let mut props: Vec<vk::QueueFamilyProperties2> = video
            .iter_mut()
            .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
            .collect();
        instance.get_physical_device_queue_family_properties2(physical, &mut props);
        let flags: Vec<vk::QueueFlags> = props
            .iter()
            .map(|p| p.queue_family_properties.queue_flags)
            .collect();
        drop(props);
        let find = |kind: vk::QueueFlags| {
            flags
                .iter()
                .zip(&video)
                .position(|(f, v)| f.contains(kind) && !v.video_codec_operations.is_empty())
                .map(|i| (i as u32, video[i].video_codec_operations))
        };
        (
            find(vk::QueueFlags::VIDEO_DECODE_KHR),
            find(vk::QueueFlags::VIDEO_ENCODE_KHR),
        )
    }
}

/// Which of the features we use the device has.
///
/// # Safety
/// `physical` belongs to `instance`.
unsafe fn supported_features(instance: &ash::Instance, physical: vk::PhysicalDevice) -> Features {
    let mut v11 = vk::PhysicalDeviceVulkan11Features::default();
    let mut v12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut v13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut all = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut v11)
        .push_next(&mut v12)
        .push_next(&mut v13);
    // SAFETY: as the caller promises.
    unsafe { instance.get_physical_device_features2(physical, &mut all) };
    Features {
        sampler_ycbcr_conversion: v11.sampler_ycbcr_conversion == vk::TRUE,
        timeline_semaphore: v12.timeline_semaphore == vk::TRUE,
        synchronization2: v13.synchronization2 == vk::TRUE,
    }
}

/// Device kinds from best to worst for video.
fn rank(kind: vk::PhysicalDeviceType) -> u32 {
    match kind {
        vk::PhysicalDeviceType::DISCRETE_GPU => 0,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
        vk::PhysicalDeviceType::CPU => 3,
        _ => 4,
    }
}

impl Gpu {
    /// For offscreen rendering (tests, diagnostics).
    pub fn new() -> Result<Self, String> {
        Self::create(&[], false, Prefer::Best)
    }

    /// Offscreen, on the preferred GPU if Vulkan has it.
    pub fn preferring(prefer: Prefer) -> Result<Self, String> {
        Self::create(&[], false, prefer)
    }

    /// `instance_extensions` are what the window system needs for a
    /// surface; `present` enables the swapchain extension.
    pub fn create(
        instance_extensions: &[*const c_char],
        present: bool,
        prefer: Prefer,
    ) -> Result<Self, String> {
        // SAFETY: loading the system Vulkan loader; every object created
        // below is destroyed in Drop or on the error path.
        unsafe {
            log::debug!("loading the Vulkan loader");
            let entry = ash::Entry::load().map_err(|e| format!("no Vulkan loader: {e}"))?;
            log::debug!("creating the instance");
            let app = vk::ApplicationInfo::default()
                .application_name(c"fernsicht")
                .api_version(vk::API_VERSION_1_3);
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default()
                        .application_info(&app)
                        .enabled_extension_names(instance_extensions),
                    None,
                )
                .map_err(|e| format!("create Vulkan instance: {e}"))?;
            log::debug!("instance created");
            let names = instance_extensions
                .iter()
                .map(|&n| CStr::from_ptr(n).to_owned())
                .collect();
            match Self::with_instance(entry, instance.clone(), names, present, prefer) {
                Ok(gpu) => Ok(gpu),
                Err(e) => {
                    instance.destroy_instance(None);
                    Err(e)
                }
            }
        }
    }

    unsafe fn with_instance(
        entry: ash::Entry,
        instance: ash::Instance,
        instance_extensions: Vec<std::ffi::CString>,
        present: bool,
        prefer: Prefer,
    ) -> Result<Self, String> {
        // SAFETY: instance is valid; see create().
        unsafe {
            let wanted = std::env::var(DEVICE_ENV).ok().map(|s| s.to_lowercase());
            log::debug!("enumerating devices");
            let mut candidates = Vec::new();
            for pd in instance
                .enumerate_physical_devices()
                .map_err(|e| format!("list Vulkan devices: {e}"))?
            {
                let props = instance.get_physical_device_properties(pd);
                let name = props
                    .device_name_as_c_str()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if props.api_version < vk::API_VERSION_1_3 {
                    continue;
                }
                if wanted
                    .as_ref()
                    .is_some_and(|w| !name.to_lowercase().contains(w))
                {
                    continue;
                }
                let family = instance
                    .get_physical_device_queue_family_properties(pd)
                    .iter()
                    .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS));
                if let Some(family) = family {
                    let has_drm = instance
                        .enumerate_device_extension_properties(pd)
                        .unwrap_or_default()
                        .iter()
                        .any(|e| {
                            e.extension_name_as_c_str() == Ok(ash::ext::physical_device_drm::NAME)
                        });
                    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
                    let mut ids = vk::PhysicalDeviceIDProperties::default();
                    let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut ids);
                    if has_drm {
                        props2 = props2.push_next(&mut drm);
                    }
                    instance.get_physical_device_properties2(pd, &mut props2);
                    let render = (has_drm && drm.has_render == vk::TRUE)
                        .then_some((drm.render_major as u32, drm.render_minor as u32));
                    let candidate = Candidate {
                        rank: rank(props.device_type),
                        render,
                        uuid: ids.device_uuid,
                    };
                    candidates.push((candidate, pd, family as u32, name));
                }
            }
            log::debug!(
                "candidates: {:?}",
                candidates.iter().map(|c| (&c.3, c.0)).collect::<Vec<_>>()
            );
            let chosen = pick(&candidates.iter().map(|c| c.0).collect::<Vec<_>>(), &prefer);
            if prefer != Prefer::Best
                && let Some(i) = chosen
                && !prefer.matches(&candidates[i].0)
            {
                log::warn!("no Vulkan device for {prefer:?}; using {}", candidates[i].3);
            }
            let Some((_, physical, queue_family, name)) = chosen.map(|i| candidates.swap_remove(i))
            else {
                return Err(match wanted {
                    Some(w) => format!("no Vulkan 1.3 device matching {DEVICE_ENV}={w}"),
                    None => "no Vulkan 1.3 device with graphics".into(),
                });
            };

            let available: Vec<String> = instance
                .enumerate_device_extension_properties(physical)
                .map_err(|e| format!("list device extensions: {e}"))?
                .iter()
                .filter_map(|e| e.extension_name_as_c_str().ok())
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            let has = |n: &CStr| available.iter().any(|a| a.as_bytes() == n.to_bytes());
            let dmabuf = DMABUF_EXTENSIONS.iter().all(|n| has(n));
            let mut extensions: Vec<*const c_char> = Vec::new();
            if dmabuf {
                extensions.extend(DMABUF_EXTENSIONS.iter().map(|n| n.as_ptr()));
            }
            if present {
                if !has(ash::khr::swapchain::NAME) {
                    return Err(format!("{name} cannot present (no swapchain support)"));
                }
                extensions.push(ash::khr::swapchain::NAME.as_ptr());
            }

            // Video queues, and what the codecs need besides.
            let (decode, encode) = video_families(&instance, physical);
            let decode = decode.filter(|_| has(DECODE_EXTENSIONS[0]) && has(VIDEO_EXTENSIONS[0]));
            let encode = encode.filter(|_| has(ENCODE_EXTENSIONS[0]) && has(VIDEO_EXTENSIONS[0]));
            let mut video: Vec<&'static CStr> = Vec::new();
            if decode.is_some() || encode.is_some() {
                video.extend(VIDEO_EXTENSIONS.iter().filter(|n| has(n)));
            }
            if decode.is_some() {
                video.extend(DECODE_EXTENSIONS.iter().filter(|n| has(n)));
            }
            if encode.is_some() {
                video.extend(ENCODE_EXTENSIONS.iter().filter(|n| has(n)));
            }
            extensions.extend(video.iter().map(|n| n.as_ptr()));
            let mut device_extensions: Vec<&'static CStr> = Vec::new();
            if dmabuf {
                device_extensions.extend(DMABUF_EXTENSIONS);
            }
            if present {
                device_extensions.push(ash::khr::swapchain::NAME);
            }
            device_extensions.extend(&video);

            let features = supported_features(&instance, physical);
            let priorities = [1.0];
            let mut families = vec![queue_family];
            for (family, _) in decode.iter().chain(&encode) {
                if !families.contains(family) {
                    families.push(*family);
                }
            }
            let queues: Vec<vk::DeviceQueueCreateInfo> = families
                .into_iter()
                .map(|family| {
                    vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(family)
                        .queue_priorities(&priorities)
                })
                .collect();
            // The NVIDIA driver now and then refuses a device right after
            // another one was released (seen in CI: about 1 in 25 runs).
            // It works a moment later, so try a few times.
            let mut attempt = 1;
            let device = loop {
                // Fresh each attempt: push_next links them into a chain,
                // and linking them again would make it a cycle.
                let mut v11 = vk::PhysicalDeviceVulkan11Features::default()
                    .sampler_ycbcr_conversion(features.sampler_ycbcr_conversion);
                let mut v12 = vk::PhysicalDeviceVulkan12Features::default()
                    .timeline_semaphore(features.timeline_semaphore);
                let mut v13 = vk::PhysicalDeviceVulkan13Features::default()
                    .dynamic_rendering(true)
                    .synchronization2(features.synchronization2);
                let created = instance.create_device(
                    physical,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&queues)
                        .enabled_extension_names(&extensions)
                        .push_next(&mut v11)
                        .push_next(&mut v12)
                        .push_next(&mut v13),
                    None,
                );
                match created {
                    Ok(device) => break device,
                    Err(e) if attempt < DEVICE_ATTEMPTS && device_error_is_transient(e) => {
                        log::warn!("creating the Vulkan device on {name} failed ({e}), retrying");
                        std::thread::sleep(std::time::Duration::from_millis(50 * attempt));
                        attempt += 1;
                    }
                    Err(e) => return Err(format!("create Vulkan device on {name}: {e}")),
                }
            };
            log::debug!("device created on {name}");
            let queue = device.get_device_queue(queue_family, 0);
            let mut ids = vk::PhysicalDeviceIDProperties::default();
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut ids);
            instance.get_physical_device_properties2(physical, &mut props2);
            let uuid = ids.device_uuid;
            if let Some((family, _)) = decode {
                log::debug!("video decode on queue family {family}");
            }
            let video_queue = |(family, operations)| VideoQueue {
                family,
                operations,
                lock: QueueLock::default(),
            };
            Ok(Self {
                decode: decode.map(video_queue),
                encode: encode.map(video_queue),
                instance_extensions,
                device_extensions,
                features,
                uuid,
                memory: instance.get_physical_device_memory_properties(physical),
                memory_fd: dmabuf
                    .then(|| ash::khr::external_memory_fd::Device::new(&instance, &device)),
                swapchain: present.then(|| ash::khr::swapchain::Device::new(&instance, &device)),
                entry,
                instance,
                physical,
                device,
                queue,
                queue_lock: QueueLock::default(),
                queue_family,
                name,
                lost: std::sync::atomic::AtomicBool::new(false),
            })
        }
    }

    /// The device's name, e.g. "AMD Radeon RX 7800 XT (RADV NAVI32)".
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The GPU's UUID, the same CUDA reports for it.
    pub fn device_uuid(&self) -> [u8; 16] {
        self.uuid
    }

    /// Whether DMA-BUFs can be imported (the extensions are there; a given
    /// buffer may still be refused).
    pub fn can_import_dmabuf(&self) -> bool {
        self.memory_fd.is_some()
    }

    /// Whether the GPU stopped finishing work: nothing is submitted to it
    /// or waited for any more.
    pub fn is_lost(&self) -> bool {
        self.lost.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Gives the GPU up: work it was given did not finish in time (a hung
    /// or faulted GPU; the NVIDIA driver reports such faults as Xid in the
    /// kernel log, and the work then never finishes) or the device is
    /// lost. From now on submissions fail at once, waiting for the GPU
    /// returns at once, and objects it may still use are left alone
    /// instead of destroyed: a stuck process would be worse than a leak.
    pub fn give_up(&self, why: &str) {
        if !self.lost.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::error!("{}: {why}; giving the GPU up", self.name);
        }
    }

    /// Waits until the GPU is done with everything submitted (at once for
    /// a GPU given up). Holds the queue lock: Vulkan requires the queues
    /// to be left alone meanwhile.
    pub fn wait_idle(&self) {
        if self.is_lost() {
            return;
        }
        let _queue = self.queue_lock.lock();
        let mut video: Vec<&QueueLock> = Vec::new();
        for q in [&self.decode, &self.encode].into_iter().flatten() {
            if let Some(lock) = self.queue_lock_for(q.family)
                && !std::ptr::eq(lock, &self.queue_lock)
                && !video.iter().any(|l| std::ptr::eq(*l, lock))
            {
                video.push(lock);
            }
        }
        let _video: Vec<QueueGuard<'_>> = video.into_iter().map(QueueLock::lock).collect();
        // SAFETY: valid device; the queues are not used concurrently.
        if unsafe { self.device.device_wait_idle() } == Err(vk::Result::ERROR_DEVICE_LOST) {
            self.give_up("device lost");
        }
    }

    /// The lock of the (first) queue of `family`; one queue of a family is
    /// created, so families that coincide share it.
    pub fn queue_lock_for(&self, family: u32) -> Option<&QueueLock> {
        if family == self.queue_family {
            return Some(&self.queue_lock);
        }
        [&self.decode, &self.encode]
            .into_iter()
            .flatten()
            .find(|q| q.family == family)
            .map(|q| &q.lock)
    }

    /// A memory type allowed by `bits` with all of `flags`.
    pub fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.memory.memory_type_count).find(|&i| {
            bits & (1 << i) != 0
                && self.memory.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
        })
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        if self.is_lost() {
            // Its work may never finish: leave the device to the process's
            // end rather than wait for it.
            return;
        }
        // SAFETY: everything created from the device is destroyed by its
        // owners first (they hold an Arc<Gpu>).
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD: Candidate = Candidate {
        rank: 0,
        render: Some((226, 129)),
        uuid: [1; 16],
    };
    const INTEL: Candidate = Candidate {
        rank: 1,
        render: Some((226, 128)),
        uuid: [2; 16],
    };
    const LLVMPIPE: Candidate = Candidate {
        rank: 3,
        render: None,
        uuid: [3; 16],
    };

    #[test]
    fn the_preferred_gpu_wins_over_the_better_kind() {
        let gpus = [LLVMPIPE, INTEL, AMD];
        assert_eq!(pick(&gpus, &Prefer::Best), Some(2), "discrete first");
        // The VAAPI decoder runs on the iGPU: its pictures import there.
        assert_eq!(pick(&gpus, &Prefer::RenderNode(226, 128)), Some(1));
        assert_eq!(pick(&gpus, &Prefer::Uuid([1; 16])), Some(2));
        // Not there: the best kind.
        assert_eq!(pick(&gpus, &Prefer::RenderNode(226, 200)), Some(2));
        assert_eq!(pick(&[], &Prefer::Best), None);
    }

    #[test]
    fn device_numbers_split_like_glibc() {
        // makedev(226, 128) and a large one, makedev(4097, 300000).
        assert_eq!(dev_numbers(0xe280), (226, 128));
        let big = (300000u64 & 0xff)
            | ((4097u64 & 0xfff) << 8)
            | ((300000u64 & !0xff) << 12)
            | ((4097u64 & !0xfff) << 32);
        assert_eq!(dev_numbers(big), (4097, 300000));
    }

    #[test]
    fn a_queue_lock_taken_by_one_call_waits_for_the_release() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let lock = Arc::new(QueueLock::default());
        // As FFmpeg does: lock_queue, submit, unlock_queue.
        lock.acquire();
        let entered = Arc::new(AtomicBool::new(false));
        let other = std::thread::spawn({
            let (lock, entered) = (lock.clone(), entered.clone());
            move || {
                let _guard = lock.lock();
                entered.store(true, Ordering::SeqCst);
            }
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !entered.load(Ordering::SeqCst),
            "the queue was still locked"
        );
        lock.release();
        other.join().unwrap();
        assert!(entered.load(Ordering::SeqCst));
        // The guard gave it back.
        drop(lock.lock());
    }

    #[test]
    fn only_transient_device_errors_are_retried() {
        assert!(device_error_is_transient(
            vk::Result::ERROR_INITIALIZATION_FAILED
        ));
        assert!(device_error_is_transient(
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY
        ));
        assert!(!device_error_is_transient(
            vk::Result::ERROR_EXTENSION_NOT_PRESENT
        ));
        assert!(!device_error_is_transient(
            vk::Result::ERROR_FEATURE_NOT_PRESENT
        ));
    }
}
