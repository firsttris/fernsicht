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

pub struct Gpu {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    /// Vulkan requires submissions to one queue to be serialized; several
    /// renderers may share a device.
    pub queue_lock: std::sync::Mutex<()>,
    pub queue_family: u32,
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
            match Self::with_instance(entry, instance.clone(), present, prefer) {
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

            let priorities = [1.0];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family)
                .queue_priorities(&priorities)];
            let mut v13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true);
            // The NVIDIA driver now and then refuses a device right after
            // another one was released (seen in CI: about 1 in 25 runs).
            // It works a moment later, so try a few times.
            let mut attempt = 1;
            let device = loop {
                let created = instance.create_device(
                    physical,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&queues)
                        .enabled_extension_names(&extensions)
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
            Ok(Self {
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
                queue_lock: std::sync::Mutex::new(()),
                queue_family,
                name,
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

    /// Waits until the GPU is done with everything submitted. Holds the
    /// queue lock: Vulkan requires the queues to be left alone meanwhile.
    pub fn wait_idle(&self) {
        let _queue = self.queue_lock.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: valid device; the queues are not used concurrently.
        let _ = unsafe { self.device.device_wait_idle() };
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
