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

/// Picks the device whose name contains this (case-insensitive), e.g.
/// `llvmpipe` for the software rasterizer.
pub const DEVICE_ENV: &str = "FERNSICHT_VULKAN_DEVICE";

pub struct Gpu {
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) entry: ash::Entry,
    pub(crate) instance: ash::Instance,
    pub(crate) physical: vk::PhysicalDevice,
    pub(crate) device: ash::Device,
    pub(crate) queue: vk::Queue,
    pub(crate) queue_family: u32,
    memory: vk::PhysicalDeviceMemoryProperties,
    /// Present when the DMA-BUF import extensions are enabled.
    pub(crate) memory_fd: Option<ash::khr::external_memory_fd::Device>,
    /// Present when created for a window.
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) swapchain: Option<ash::khr::swapchain::Device>,
    name: String,
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
        Self::create(&[], false)
    }

    /// `instance_extensions` are what the window system needs for a
    /// surface; `present` enables the swapchain extension.
    #[cfg_attr(not(feature = "window"), allow(dead_code))]
    pub(crate) fn create(
        instance_extensions: &[*const c_char],
        present: bool,
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
            match Self::with_instance(entry, instance.clone(), present) {
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
                    candidates.push((rank(props.device_type), pd, family as u32, name));
                }
            }
            candidates.sort_by_key(|c| c.0);
            log::debug!(
                "candidates: {:?}",
                candidates.iter().map(|c| &c.3).collect::<Vec<_>>()
            );
            let Some((_, physical, queue_family, name)) = candidates.into_iter().next() else {
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
            let device = instance
                .create_device(
                    physical,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&queues)
                        .enabled_extension_names(&extensions)
                        .push_next(&mut v13),
                    None,
                )
                .map_err(|e| format!("create Vulkan device on {name}: {e}"))?;
            log::debug!("device created on {name}");
            let queue = device.get_device_queue(queue_family, 0);
            Ok(Self {
                memory: instance.get_physical_device_memory_properties(physical),
                memory_fd: dmabuf
                    .then(|| ash::khr::external_memory_fd::Device::new(&instance, &device)),
                swapchain: present.then(|| ash::khr::swapchain::Device::new(&instance, &device)),
                entry,
                instance,
                physical,
                device,
                queue,
                queue_family,
                name,
            })
        }
    }

    /// The device's name, e.g. "AMD Radeon RX 7800 XT (RADV NAVI32)".
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether DMA-BUFs can be imported (the extensions are there; a given
    /// buffer may still be refused).
    pub fn can_import_dmabuf(&self) -> bool {
        self.memory_fd.is_some()
    }

    /// A memory type allowed by `bits` with all of `flags`.
    pub(crate) fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Option<u32> {
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
