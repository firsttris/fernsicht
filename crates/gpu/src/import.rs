//! Vulkan images and buffers: plain ones, and DMA-BUF planes imported
//! without a copy.

use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

use ash::vk;
use fernsicht_capture::DmaBuf;
use fernsicht_capture::dmabuf::{formats, fourcc_name};

use crate::Gpu;

fn err(what: &'static str) -> impl Fn(vk::Result) -> String {
    move |e| format!("{what}: {e}")
}

/// An image that owns its memory and view.
pub struct Plane {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

pub fn destroy(gpu: &Gpu, p: Plane) {
    // SAFETY: the GPU no longer uses the plane; owned by the caller.
    unsafe {
        gpu.device.destroy_image_view(p.view, None);
        gpu.device.destroy_image(p.image, None);
        gpu.device.free_memory(p.memory, None);
    }
}

/// A 2D image in device-local memory.
pub fn device_image(
    gpu: &Gpu,
    format: vk::Format,
    width: u32,
    height: u32,
    usage: vk::ImageUsageFlags,
) -> Result<(vk::Image, vk::DeviceMemory), String> {
    let d = &gpu.device;
    // SAFETY: object creation on a valid device; freed on error.
    unsafe {
        let image = d
            .create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(usage)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
            .map_err(err("create image"))?;
        let req = d.get_image_memory_requirements(image);
        let Some(ty) = gpu.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
        else {
            d.destroy_image(image, None);
            return Err("no device-local memory for an image".into());
        };
        let memory = match d.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ty),
            None,
        ) {
            Ok(m) => m,
            Err(e) => {
                d.destroy_image(image, None);
                return Err(format!("allocate image memory: {e}"));
            }
        };
        if let Err(e) = d.bind_image_memory(image, memory, 0) {
            d.destroy_image(image, None);
            d.free_memory(memory, None);
            return Err(format!("bind image memory: {e}"));
        }
        Ok((image, memory))
    }
}

pub fn view(gpu: &Gpu, image: vk::Image, format: vk::Format) -> Result<vk::ImageView, String> {
    // SAFETY: image is valid.
    unsafe {
        gpu.device
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
            .map_err(err("create image view"))
    }
}

/// A host-visible, coherent buffer, persistently mapped.
pub fn host_buffer(
    gpu: &Gpu,
    size: u64,
    usage: vk::BufferUsageFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8), String> {
    let d = &gpu.device;
    // SAFETY: object creation on a valid device; freed on error.
    unsafe {
        let buffer = d
            .create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(usage),
                None,
            )
            .map_err(err("create buffer"))?;
        let req = d.get_buffer_memory_requirements(buffer);
        let flags = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        // The CPU reads what the GPU wrote into a readback buffer: cached
        // memory makes that ~10× faster than write-combined memory.
        let cached = usage
            .contains(vk::BufferUsageFlags::TRANSFER_DST)
            .then(|| {
                gpu.memory_type(
                    req.memory_type_bits,
                    flags | vk::MemoryPropertyFlags::HOST_CACHED,
                )
            })
            .flatten();
        let Some(ty) = cached.or_else(|| gpu.memory_type(req.memory_type_bits, flags)) else {
            d.destroy_buffer(buffer, None);
            return Err("no host-visible memory".into());
        };
        let memory = match d.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ty),
            None,
        ) {
            Ok(m) => m,
            Err(e) => {
                d.destroy_buffer(buffer, None);
                return Err(format!("allocate buffer memory: {e}"));
            }
        };
        let mapped = d
            .bind_buffer_memory(buffer, memory, 0)
            .and_then(|()| d.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()));
        match mapped {
            Ok(p) => Ok((buffer, memory, p.cast())),
            Err(e) => {
                d.destroy_buffer(buffer, None);
                d.free_memory(memory, None);
                Err(format!("map buffer: {e}"))
            }
        }
    }
}

/// Imports an NV12 DMA-BUF as two images, Y (`R8`) and UV (`R8G8`).
pub fn nv12(gpu: &Gpu, image: &DmaBuf) -> Result<[Plane; 2], String> {
    if image.fourcc != formats::NV12 {
        return Err(format!("expected NV12, got {}", fourcc_name(image.fourcc)));
    }
    if image.planes.len() != 2 {
        return Err(format!(
            "NV12 in {} planes (compressed layout?) is not supported",
            image.planes.len()
        ));
    }
    image.validate().map_err(|e| e.to_string())?;
    let (w, h) = (image.width, image.height);
    let y = plane(gpu, image, &[0], vk::Format::R8_UNORM, w, h)?;
    match plane(gpu, image, &[1], vk::Format::R8G8_UNORM, w / 2, h / 2) {
        Ok(uv) => Ok([y, uv]),
        Err(e) => {
            destroy(gpu, y);
            Err(e)
        }
    }
}

/// The Vulkan format with the memory layout of an RGB DRM format. Alpha is
/// ignored by the users, as on scanout.
pub fn rgb_format(fourcc: u32) -> Option<vk::Format> {
    Some(match fourcc {
        // B, G, R, X in memory.
        formats::XRGB8888 | formats::ARGB8888 => vk::Format::B8G8R8A8_UNORM,
        formats::XBGR8888 | formats::ABGR8888 => vk::Format::R8G8B8A8_UNORM,
        // 32-bit words, blue (resp. red) in the low bits.
        formats::XRGB2101010 | formats::ARGB2101010 => vk::Format::A2R10G10B10_UNORM_PACK32,
        formats::XBGR2101010 | formats::ABGR2101010 => vk::Format::A2B10G10R10_UNORM_PACK32,
        _ => return None,
    })
}

/// Imports an RGB DMA-BUF (8 or 10 bit, see [`rgb_format`]) as one image
/// for sampling. Extra planes of a compressed layout (in the same buffer
/// object) are passed on to the driver.
pub fn rgb(gpu: &Gpu, image: &DmaBuf) -> Result<Plane, String> {
    let Some(format) = rgb_format(image.fourcc) else {
        return Err(format!(
            "DMA-BUF format {} is not supported",
            fourcc_name(image.fourcc)
        ));
    };
    image.validate().map_err(|e| e.to_string())?;
    if image
        .planes
        .iter()
        .any(|p| p.object != image.planes[0].object)
    {
        return Err("RGB DMA-BUF with planes in several buffer objects is not supported".into());
    }
    let planes: Vec<usize> = (0..image.planes.len()).collect();
    plane(gpu, image, &planes, format, image.width, image.height)
}

/// Imports the given planes of `image` (all in one buffer object) as one
/// Vulkan image: one plane for NV12's Y or UV, the image plus its metadata
/// planes for a compressed RGB layout.
fn plane(
    gpu: &Gpu,
    image: &DmaBuf,
    planes: &[usize],
    format: vk::Format,
    width: u32,
    height: u32,
) -> Result<Plane, String> {
    let Some(memory_fd) = &gpu.memory_fd else {
        return Err(format!("{} has no DMA-BUF import", gpu.name()));
    };
    let src = image.planes[planes[0]];
    let d = &gpu.device;
    let usage = vk::ImageUsageFlags::SAMPLED;
    // SAFETY: Vulkan calls on valid objects; everything created is freed on
    // the error paths, and the imported descriptor is a duplicate that
    // Vulkan owns once the allocation succeeds.
    unsafe {
        // Can this format be sampled with this modifier at all?
        let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(image.modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let query = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .push_next(&mut modifier_info)
            .push_next(&mut external_info);
        let mut props = vk::ImageFormatProperties2::default();
        gpu.instance
            .get_physical_device_image_format_properties2(gpu.physical, &query, &mut props)
            .map_err(|e| {
                format!(
                    "{} cannot sample {format:?} with modifier {:#x}: {e}",
                    gpu.name(),
                    image.modifier
                )
            })?;

        let layouts: Vec<vk::SubresourceLayout> = planes
            .iter()
            .map(|&i| vk::SubresourceLayout {
                offset: u64::from(image.planes[i].offset),
                size: 0,
                row_pitch: u64::from(image.planes[i].pitch),
                array_pitch: 0,
                depth_pitch: 0,
            })
            .collect();
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(image.modifier)
            .plane_layouts(&layouts);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let vk_image = d
            .create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(usage)
                    .initial_layout(vk::ImageLayout::UNDEFINED)
                    .push_next(&mut external)
                    .push_next(&mut explicit),
                None,
            )
            .map_err(err("create image for the DMA-BUF"))?;

        let fd: OwnedFd = match image.objects[src.object].try_clone() {
            Ok(fd) => fd,
            Err(e) => {
                d.destroy_image(vk_image, None);
                return Err(format!("dup DMA-BUF: {e}"));
            }
        };
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        let req = d.get_image_memory_requirements(vk_image);
        let bits = memory_fd
            .get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.as_raw_fd(),
                &mut fd_props,
            )
            .map(|()| fd_props.memory_type_bits & req.memory_type_bits);
        let ty = match bits {
            Ok(bits) if bits != 0 => bits.trailing_zeros(),
            other => {
                d.destroy_image(vk_image, None);
                return Err(format!("no memory type for the DMA-BUF ({other:?})"));
            }
        };
        let raw = fd.into_raw_fd();
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(vk_image);
        let memory = match d.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ty)
                .push_next(&mut import)
                .push_next(&mut dedicated),
            None,
        ) {
            Ok(m) => m,
            Err(e) => {
                // Not consumed on failure: close it ourselves.
                drop(OwnedFd::from_raw_fd(raw));
                d.destroy_image(vk_image, None);
                return Err(format!("import DMA-BUF memory: {e}"));
            }
        };
        if let Err(e) = d.bind_image_memory(vk_image, memory, 0) {
            d.destroy_image(vk_image, None);
            d.free_memory(memory, None);
            return Err(format!("bind DMA-BUF memory: {e}"));
        }
        match view(gpu, vk_image, format) {
            Ok(view) => Ok(Plane {
                image: vk_image,
                memory,
                view,
            }),
            Err(e) => {
                d.destroy_image(vk_image, None);
                d.free_memory(memory, None);
                Err(e)
            }
        }
    }
}
