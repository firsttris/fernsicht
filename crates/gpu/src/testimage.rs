//! RGB pictures exported as DMA-BUFs the way a display server allocates
//! them (driver-chosen tiling modifier), for testing the import paths
//! without the privileges KMS capture needs.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;

use ash::vk;
use fernsicht_capture::dmabuf::fourcc_name;
use fernsicht_capture::{DmaBuf, DmaBufPlane};

use crate::Gpu;
use crate::convert::submit_and_wait;
use crate::import::{self, rgb_format};

fn err(what: &'static str) -> impl Fn(vk::Result) -> String {
    move |e| format!("{what}: {e}")
}

/// Single-plane modifiers the GPU can sample and upload to for `format`.
fn modifiers(gpu: &Gpu, format: vk::Format) -> Vec<u64> {
    // SAFETY: queries on a valid physical device.
    unsafe {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        gpu.instance
            .get_physical_device_format_properties2(gpu.physical, format, &mut props);
        let mut entries = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut entries);
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        gpu.instance
            .get_physical_device_format_properties2(gpu.physical, format, &mut props);
        let need = vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::TRANSFER_DST;
        entries
            .iter()
            .filter(|m| {
                m.drm_format_modifier_plane_count == 1
                    && m.drm_format_modifier_tiling_features.contains(need)
            })
            .map(|m| m.drm_format_modifier)
            .collect()
    }
}

/// Uploads `pixels` (4 bytes per pixel, tightly packed, in the memory
/// layout of the DRM `fourcc`) into a new GPU image and exports it as a
/// DMA-BUF. The image lives as long as the returned descriptors.
pub fn upload_as_dmabuf(
    gpu: &Arc<Gpu>,
    width: u32,
    height: u32,
    fourcc: u32,
    pixels: &[u8],
) -> Result<DmaBuf, String> {
    let format = rgb_format(fourcc)
        .ok_or_else(|| format!("{} is not an RGB format", fourcc_name(fourcc)))?;
    let len = width as usize * height as usize * 4;
    if width == 0 || height == 0 || pixels.len() < len {
        return Err("picture smaller than width×height×4".into());
    }
    let fd_api = gpu.memory_fd.as_ref().ok_or("no DMA-BUF support")?;
    let mods = modifiers(gpu, format);
    if mods.is_empty() {
        return Err(format!("{} has no modifier for {format:?}", gpu.name()));
    }
    let modifier_api = ash::ext::image_drm_format_modifier::Device::new(&gpu.instance, &gpu.device);
    let d = &gpu.device;
    let handle = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
    // SAFETY: Vulkan calls on valid objects; everything but the exported
    // descriptor is destroyed before returning.
    unsafe {
        let mut list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&mods);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(handle);
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
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                    .initial_layout(vk::ImageLayout::UNDEFINED)
                    .push_next(&mut external)
                    .push_next(&mut list),
                None,
            )
            .map_err(err("create image"))?;
        let mut memory = vk::DeviceMemory::null();
        let mut staging = None;
        let mut cmd_pool = vk::CommandPool::null();
        let mut fence = vk::Fence::null();
        let result = (|| {
            let req = d.get_image_memory_requirements(image);
            let ty = gpu
                .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .ok_or("no device-local memory")?;
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(handle);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            memory = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(ty)
                        .push_next(&mut export)
                        .push_next(&mut dedicated),
                    None,
                )
                .map_err(err("allocate image memory"))?;
            d.bind_image_memory(image, memory, 0)
                .map_err(err("bind image memory"))?;

            let (buffer, buffer_mem, mapped) =
                import::host_buffer(gpu, len as u64, vk::BufferUsageFlags::TRANSFER_SRC)?;
            staging = Some((buffer, buffer_mem));
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), mapped, len);

            cmd_pool = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family),
                    None,
                )
                .map_err(err("command pool"))?;
            let cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(cmd_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(err("command buffer"))?[0];
            fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(err("fence"))?;
            d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .map_err(err("begin"))?;
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            let to_dst = vk::ImageMemoryBarrier::default()
                .image(image)
                .subresource_range(range)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED);
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            d.cmd_copy_buffer_to_image(
                cmd,
                buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })],
            );
            // Hand it to the "foreign" consumer, as a display server would.
            let release = vk::ImageMemoryBarrier::default()
                .image(image)
                .subresource_range(range)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(gpu.queue_family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT);
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release],
            );
            d.end_command_buffer(cmd).map_err(err("end"))?;
            submit_and_wait(gpu, cmd, fence)?;

            let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
            modifier_api
                .get_image_drm_format_modifier_properties(image, &mut props)
                .map_err(err("query modifier"))?;
            let layout = d.get_image_subresource_layout(
                image,
                vk::ImageSubresource::default()
                    .aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            );
            let fd = fd_api
                .get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(memory)
                        .handle_type(handle),
                )
                .map_err(err("export DMA-BUF"))?;
            Ok(DmaBuf {
                width,
                height,
                fourcc,
                modifier: props.drm_format_modifier,
                objects: vec![Arc::new(OwnedFd::from_raw_fd(fd))],
                planes: vec![DmaBufPlane {
                    object: 0,
                    offset: layout.offset as u32,
                    pitch: layout.row_pitch as u32,
                }],
            })
        })();
        d.destroy_fence(fence, None);
        d.destroy_command_pool(cmd_pool, None);
        if let Some((buffer, buffer_mem)) = staging {
            d.destroy_buffer(buffer, None);
            d.free_memory(buffer_mem, None);
        }
        // The exported descriptor keeps the memory alive.
        d.destroy_image(image, None);
        d.free_memory(memory, None);
        result
    }
}
