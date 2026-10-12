//! RGB DMA-BUF → NV12 for hardware encoders that take neither RGB nor
//! foreign DMA-BUFs directly (NVENC, Vulkan Video).
//!
//! A compute shader samples the imported picture (scaling it to the stream
//! size) and writes NV12 or P010 (limited range, the matrix of the
//! stream's [`Colour`]) into a linear buffer. For
//! NVENC its memory is exported as an opaque file descriptor: CUDA imports
//! that buffer once and copies each frame into the encoder's input on the
//! GPU. For Vulkan Video the buffer stays on the device and the caller
//! copies it into the encoder's image in the same submission
//! ([`Converter::convert_with`]).

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;

use ash::vk;
use fernsicht_capture::DmaBuf;
use fernsicht_capture::dmabuf::BufferIdentity;
use fernsicht_core::colour::{Colour, Depth};

use crate::Gpu;
use crate::device::{Timeline, err, submit_and_wait, submit_timeline, wait_fence};
use crate::import;

/// The conversion shader (`convert.wgsl`), compiled by the build script.
const SPIRV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/convert.spv"));

/// Where the planes are in the output buffer (NV12, or P010 at 10 bit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nv12Layout {
    pub width: u32,
    pub height: u32,
    pub depth: Depth,
    /// Bytes per row of both planes.
    pub pitch: u32,
    /// Byte offset of the UV plane.
    pub uv_offset: u64,
}

impl Nv12Layout {
    /// 8-bit NV12.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        Self::with_depth(width, height, Depth::Eight)
    }

    /// Even sizes only (4:2:0 subsamples 2×2). Rows start on 256 bytes, as
    /// in FFmpeg's CUDA frames: NVENC reads the buffer as its input.
    pub fn with_depth(width: u32, height: u32, depth: Depth) -> Result<Self, String> {
        if width < 2 || height < 2 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(format!("4:2:0 needs an even size, not {width}×{height}"));
        }
        let pitch = (width * depth.sample_bytes()).next_multiple_of(256);
        Ok(Self {
            width,
            height,
            depth,
            pitch,
            uv_offset: u64::from(pitch) * u64::from(height),
        })
    }

    /// Bytes of one row's samples (without the padding to `pitch`).
    pub fn row_bytes(&self) -> u32 {
        self.width * self.depth.sample_bytes()
    }

    /// Bytes of both planes.
    pub fn len(&self) -> u64 {
        self.uv_offset * 3 / 2
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The conversion shader's SPIR-V words.
pub fn spirv() -> Vec<u32> {
    ash::util::read_spv(&mut std::io::Cursor::new(SPIRV)).expect("SPIR-V from the build script")
}

pub struct Converter {
    gpu: Arc<Gpu>,
    layout: Nv12Layout,
    colour: Colour,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    sampler: vk::Sampler,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    output: vk::Buffer,
    output_memory: vk::DeviceMemory,
    output_allocation: u64,
    /// The output is exported to another API (CUDA), which takes it over
    /// after each conversion.
    exported: bool,
    /// Recently imported pictures, the most recent last.
    imports: Vec<(BufferIdentity, import::Plane)>,
}

/// Imports kept; display servers use two or three buffers per output.
const MAX_IMPORTS: usize = 4;

// SAFETY: the Vulkan handles are only used through &mut self (or &self for
// exporting the memory, which Vulkan allows from any thread); submissions
// take the GPU's queue lock.
unsafe impl Send for Converter {}

impl Converter {
    /// Converts to `width`×`height` in `colour` (its depth and matrix)
    /// into memory CUDA can import ([`Self::export_output`]). The GPU must
    /// have DMA-BUF import.
    pub fn new(gpu: Arc<Gpu>, width: u32, height: u32, colour: Colour) -> Result<Self, String> {
        Self::create(gpu, width, height, colour, true)
    }

    /// Converts to `width`×`height` in `colour` into a buffer that stays on
    /// the device, for a copy recorded with [`Self::convert_with`].
    pub fn on_device(
        gpu: Arc<Gpu>,
        width: u32,
        height: u32,
        colour: Colour,
    ) -> Result<Self, String> {
        Self::create(gpu, width, height, colour, false)
    }

    fn create(
        gpu: Arc<Gpu>,
        width: u32,
        height: u32,
        colour: Colour,
        exported: bool,
    ) -> Result<Self, String> {
        let layout = Nv12Layout::with_depth(width, height, colour.depth)?;
        if !gpu.can_import_dmabuf() {
            return Err(format!("{} cannot import DMA-BUFs", gpu.name()));
        }
        let words = spirv();
        let d = &gpu.device;
        let mut c = Self {
            gpu: gpu.clone(),
            layout,
            colour,
            set_layout: vk::DescriptorSetLayout::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            sampler: vk::Sampler::null(),
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            cmd_pool: vk::CommandPool::null(),
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            output: vk::Buffer::null(),
            output_memory: vk::DeviceMemory::null(),
            output_allocation: 0,
            exported,
            imports: Vec::new(),
        };
        // SAFETY: object creation on a valid device; Drop destroys whatever
        // was created if a later step fails.
        unsafe {
            let bindings = [
                (0, vk::DescriptorType::SAMPLED_IMAGE),
                (1, vk::DescriptorType::SAMPLER),
                (2, vk::DescriptorType::STORAGE_BUFFER),
            ]
            .map(|(binding, ty)| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(binding)
                    .descriptor_type(ty)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
            });
            c.set_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .map_err(err("descriptor layout"))?;
            let push = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(PUSH_BYTES)];
            let layouts = [c.set_layout];
            c.pipeline_layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&layouts)
                        .push_constant_ranges(&push),
                    None,
                )
                .map_err(err("pipeline layout"))?;
            let module = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                .map_err(err("shader module"))?;
            let created = d.create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .stage(
                        vk::PipelineShaderStageCreateInfo::default()
                            .stage(vk::ShaderStageFlags::COMPUTE)
                            .module(module)
                            .name(c"main"),
                    )
                    .layout(c.pipeline_layout)],
                None,
            );
            d.destroy_shader_module(module, None);
            c.pipeline = created.map_err(|(_, e)| format!("compute pipeline: {e}"))?[0];
            c.sampler = d
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::LINEAR)
                        .min_filter(vk::Filter::LINEAR)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                    None,
                )
                .map_err(err("sampler"))?;
            let sizes = [
                vk::DescriptorType::SAMPLED_IMAGE,
                vk::DescriptorType::SAMPLER,
                vk::DescriptorType::STORAGE_BUFFER,
            ]
            .map(|ty| vk::DescriptorPoolSize {
                ty,
                descriptor_count: 1,
            });
            c.pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(1)
                        .pool_sizes(&sizes),
                    None,
                )
                .map_err(err("descriptor pool"))?;
            c.set = d
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(c.pool)
                        .set_layouts(&layouts),
                )
                .map_err(err("descriptor set"))?[0];
            c.cmd_pool = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                        .queue_family_index(gpu.queue_family),
                    None,
                )
                .map_err(err("command pool"))?;
            c.cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(c.cmd_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(err("command buffer"))?[0];
            c.fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(err("fence"))?;
            c.create_output()?;
        }
        Ok(c)
    }

    /// The output buffer, in memory another API (CUDA) can import if
    /// exported.
    unsafe fn create_output(&mut self) -> Result<(), String> {
        let d = &self.gpu.device;
        let handle = vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD;
        // SAFETY: valid device; Drop frees the objects.
        unsafe {
            let mut external = vk::ExternalMemoryBufferCreateInfo::default().handle_types(handle);
            let mut info = vk::BufferCreateInfo::default()
                .size(self.layout.len())
                .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_SRC);
            if self.exported {
                info = info.push_next(&mut external);
            }
            self.output = d.create_buffer(&info, None).map_err(err("output buffer"))?;
            let req = d.get_buffer_memory_requirements(self.output);
            let ty = self
                .gpu
                .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                .ok_or("no device-local memory for the output buffer")?;
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(handle);
            let mut info = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ty);
            if self.exported {
                info = info.push_next(&mut export);
            }
            self.output_memory = d
                .allocate_memory(&info, None)
                .map_err(err("output memory"))?;
            self.output_allocation = req.size;
            d.bind_buffer_memory(self.output, self.output_memory, 0)
                .map_err(err("bind output memory"))?;
            let info = [vk::DescriptorBufferInfo::default()
                .buffer(self.output)
                .range(vk::WHOLE_SIZE)];
            d.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(self.set)
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&info)],
                &[],
            );
        }
        Ok(())
    }

    /// Whether its GPU was given up ([`Gpu::give_up`]).
    pub fn gpu_lost(&self) -> bool {
        self.gpu.is_lost()
    }

    pub fn layout(&self) -> Nv12Layout {
        self.layout
    }

    pub fn colour(&self) -> Colour {
        self.colour
    }

    /// Size of the output allocation (what an importer must be told).
    pub fn allocation_size(&self) -> u64 {
        self.output_allocation
    }

    /// A new descriptor for the output memory (for CUDA's
    /// `cuImportExternalMemory`, which takes ownership of it).
    pub fn export_output(&self) -> Result<OwnedFd, String> {
        let fd_api = self.gpu.memory_fd.as_ref().ok_or("no external memory")?;
        // SAFETY: the memory was allocated exportable as an opaque fd.
        unsafe {
            let fd = fd_api
                .get_memory_fd(
                    &vk::MemoryGetFdInfoKHR::default()
                        .memory(self.output_memory)
                        .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD),
                )
                .map_err(err("export output memory"))?;
            Ok(OwnedFd::from_raw_fd(fd))
        }
    }

    /// Reads the output buffer back into tightly packed NV12 or P010
    /// (tests and diagnostics; the encoder reads it on the GPU).
    pub fn download(&mut self) -> Result<Vec<u8>, String> {
        let l = self.layout;
        let (buffer, memory, mapped) =
            import::host_buffer(&self.gpu, l.len(), vk::BufferUsageFlags::TRANSFER_DST)?;
        let d = &self.gpu.device;
        // SAFETY: valid objects; the host buffer is freed below after the
        // GPU finished (or the device idled on error).
        let result = unsafe {
            (|| {
                let cmd = self.cmd;
                d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                    .map_err(err("reset command buffer"))?;
                d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                    .map_err(err("begin command buffer"))?;
                d.cmd_copy_buffer(
                    cmd,
                    self.output,
                    buffer,
                    &[vk::BufferCopy::default().size(l.len())],
                );
                d.end_command_buffer(cmd)
                    .map_err(err("end command buffer"))?;
                submit_and_wait(&self.gpu, &[cmd], self.fence)?;
                let raw = std::slice::from_raw_parts(mapped, l.len() as usize);
                let (w, pitch) = (l.row_bytes() as usize, l.pitch as usize);
                let mut out = Vec::with_capacity(w * l.height as usize * 3 / 2);
                for row in raw.chunks(pitch).take(l.height as usize * 3 / 2) {
                    out.extend_from_slice(&row[..w]);
                }
                Ok(out)
            })()
        };
        if result.is_err() {
            self.gpu.wait_idle();
        }
        // SAFETY: the GPU is done with the buffer (or was given up, and
        // the buffer is left to it).
        if !self.gpu.is_lost() {
            unsafe {
                d.destroy_buffer(buffer, None);
                d.free_memory(memory, None);
            }
        }
        result
    }

    /// Converts `image` into the output buffer and waits until the GPU is
    /// done, so the buffer can be read by CUDA right after.
    ///
    /// Imports are kept for the last few buffers: a display server cycles
    /// through two or three, and capture hands out a new descriptor for
    /// the same buffer every frame.
    pub fn convert(&mut self, image: &DmaBuf) -> Result<(), String> {
        self.convert_with(image, |_, _, _| {}, None)
    }

    /// [`Self::convert`], then `after` records more commands into the same
    /// submission (given the output buffer, ready to be read by
    /// transfers), which waits for and signals `timeline`. Returns when
    /// the GPU is done.
    pub fn convert_with(
        &mut self,
        image: &DmaBuf,
        after: impl FnOnce(&ash::Device, vk::CommandBuffer, vk::Buffer),
        timeline: Option<Timeline>,
    ) -> Result<(), String> {
        let key = image.identity();
        let cached = key
            .as_ref()
            .and_then(|k| self.imports.iter().position(|(c, _)| c == k));
        let plane = match cached {
            Some(i) => self.imports.remove(i).1,
            None => import::rgb(&self.gpu, image)?,
        };
        // SAFETY: the plane and our objects are valid; a plane is destroyed
        // only after the GPU finished with it (or the device idled).
        let result = unsafe { self.run(plane.image, plane.view, after, timeline) };
        if result.is_err() {
            self.gpu.wait_idle();
        }
        match (key, &result) {
            (Some(key), Ok(())) => {
                self.imports.push((key, plane));
                if self.imports.len() > MAX_IMPORTS {
                    let (_, old) = self.imports.remove(0);
                    import::destroy(&self.gpu, old);
                }
            }
            _ => import::destroy(&self.gpu, plane),
        }
        result
    }

    /// Number of buffers whose import is kept.
    pub fn cached_imports(&self) -> usize {
        self.imports.len()
    }

    unsafe fn run(
        &mut self,
        image: vk::Image,
        view: vk::ImageView,
        after: impl FnOnce(&ash::Device, vk::CommandBuffer, vk::Buffer),
        timeline: Option<Timeline>,
    ) -> Result<(), String> {
        let d = &self.gpu.device;
        let family = self.gpu.queue_family;
        // SAFETY: see convert().
        unsafe {
            let image_info = [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let sampler_info = [vk::DescriptorImageInfo::default().sampler(self.sampler)];
            d.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.set)
                        .dst_binding(0)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .image_info(&image_info),
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.set)
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::SAMPLER)
                        .image_info(&sampler_info),
                ],
                &[],
            );

            let cmd = self.cmd;
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .map_err(err("reset command buffer"))?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(err("begin command buffer"))?;
            let range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);
            // Take the picture over from its producer (the display engine);
            // GENERAL keeps its contents. An exported output buffer comes
            // back from CUDA.
            let acquire_image = vk::ImageMemoryBarrier::default()
                .image(image)
                .subresource_range(range)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(family);
            let acquire_output = vk::BufferMemoryBarrier::default()
                .buffer(self.output)
                .size(vk::WHOLE_SIZE)
                .dst_access_mask(vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                .dst_queue_family_index(family);
            let acquire_output = [acquire_output];
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                if self.exported { &acquire_output } else { &[] },
                &[acquire_image],
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[self.set],
                &[],
            );
            let l = self.layout;
            d.cmd_push_constants(
                cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &push_constants(&l, &self.colour),
            );
            // 8×8 invocations per group, each a 4×2 block.
            d.cmd_dispatch(cmd, l.width.div_ceil(32), l.height.div_ceil(16), 1);
            let release_image = vk::ImageMemoryBarrier::default()
                .image(image)
                .subresource_range(range)
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT);
            let release_output = vk::BufferMemoryBarrier::default()
                .buffer(self.output)
                .size(vk::WHOLE_SIZE)
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .src_queue_family_index(family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL);
            // Kept on the device: written, then read by the caller's
            // transfers.
            let written = vk::BufferMemoryBarrier::default()
                .buffer(self.output)
                .size(vk::WHOLE_SIZE)
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED);
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[if self.exported {
                    release_output
                } else {
                    written
                }],
                &[release_image],
            );
            after(d, cmd, self.output);
            d.end_command_buffer(cmd)
                .map_err(err("end command buffer"))?;
            submit_timeline(&self.gpu, &[cmd], None, None, timeline, self.fence)?;
            wait_fence(&self.gpu, self.fence)
        }
    }
}

/// Bytes of the shader's `Params`.
const PUSH_BYTES: u32 = 32;

/// The shader's `Params`: size, pitch, depth flag and the matrix's luma
/// weights.
pub fn push_constants(l: &Nv12Layout, colour: &Colour) -> [u8; PUSH_BYTES as usize] {
    let (kr, kb) = colour.kr_kb();
    let deep = u32::from(l.depth == Depth::Ten);
    let words = [
        l.width,
        l.height,
        l.pitch,
        deep,
        kr.to_bits(),
        kb.to_bits(),
        0,
        0,
    ];
    let mut out = [0u8; PUSH_BYTES as usize];
    for (chunk, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(words) {
        chunk.copy_from_slice(&w.to_le_bytes());
    }
    out
}

impl Drop for Converter {
    fn drop(&mut self) {
        if self.gpu.is_lost() {
            // It may still use all of it (Gpu::give_up).
            return;
        }
        self.gpu.wait_idle();
        for (_, plane) in self.imports.drain(..) {
            import::destroy(&self.gpu, plane);
        }
        let d = &self.gpu.device;
        // SAFETY: the GPU is idle; null handles are ignored by Vulkan.
        unsafe {
            d.destroy_fence(self.fence, None);
            d.destroy_command_pool(self.cmd_pool, None);
            d.destroy_descriptor_pool(self.pool, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.pipeline_layout, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_buffer(self.output, None);
            d.free_memory(self.output_memory, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_compiles() {
        assert_eq!(spirv()[0], 0x0723_0203, "SPIR-V magic");
    }

    #[test]
    fn layout_aligns_rows_to_256_bytes() {
        let l = Nv12Layout::new(1920, 1080).unwrap();
        assert_eq!(
            (l.pitch, l.uv_offset, l.len()),
            (2048, 2048 * 1080, 2048 * 1620)
        );
        let odd_rows = Nv12Layout::new(1366, 768).unwrap();
        assert_eq!(odd_rows.pitch, 1536);
        assert_eq!(Nv12Layout::new(2560, 1440).unwrap().pitch, 2560);
        assert!(Nv12Layout::new(1365, 768).is_err());
        assert!(Nv12Layout::new(0, 0).is_err());
    }

    #[test]
    fn p010_rows_hold_two_bytes_a_sample() {
        let l = Nv12Layout::with_depth(1920, 1080, Depth::Ten).unwrap();
        assert_eq!((l.pitch, l.row_bytes()), (3840, 3840));
        assert_eq!(l.len(), 3840 * 1620);
        let l = Nv12Layout::with_depth(1366, 768, Depth::Ten).unwrap();
        assert_eq!(l.pitch, 2816);
    }

    #[test]
    fn the_shader_gets_the_depth_and_the_matrix() {
        let l = Nv12Layout::with_depth(64, 32, Depth::Ten).unwrap();
        let p = push_constants(&l, &Colour::HDR10);
        let word = |i: usize| u32::from_le_bytes(p[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!([word(0), word(1), word(2), word(3)], [64, 32, 256, 1]);
        assert_eq!(f32::from_bits(word(4)), 0.2627);
        assert_eq!(f32::from_bits(word(5)), 0.0593);
        let sdr = push_constants(&Nv12Layout::new(64, 32).unwrap(), &Colour::SDR);
        assert_eq!(sdr[12..16], [0, 0, 0, 0], "8 bit");
    }
}
