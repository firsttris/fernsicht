//! NV12 → RGB on the GPU.
//!
//! A picture comes either from CPU memory (copied into two plane images
//! through a staging buffer) or as a DMA-BUF from the decoder, imported
//! once per decoder surface and sampled in place. Either way the fragment
//! shader converts BT.709 limited range to RGB while drawing a quad that
//! keeps the aspect ratio.

use std::collections::HashMap;
use std::sync::Arc;

use ash::vk;
use fernsicht_codec::Picture;

use crate::CursorOverlay;

use super::Gpu;
use super::import;

const WGSL: &str = include_str!("shader.wgsl");

/// Imported decoder surfaces kept at most; a decoder cycles through far
/// fewer, more means surfaces of an old decoder are still cached.
const MAX_IMPORTED: usize = 32;

#[derive(Debug)]
pub enum RenderError {
    /// The DMA-BUF cannot be sampled in place; CPU pictures still work.
    Import(String),
    Other(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Import(e) => write!(f, "DMA-BUF import: {e}"),
            RenderError::Other(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for RenderError {}

fn other(what: &str) -> impl Fn(vk::Result) -> RenderError + '_ {
    move |e| RenderError::Other(format!("{what}: {e}"))
}

/// Compiles the WGSL shaders to SPIR-V (both entry points in one module).
pub fn spirv() -> Result<Vec<u32>, String> {
    use naga::back::spv;
    use naga::valid::{Capabilities, ValidationFlags, Validator};
    let module = naga::front::wgsl::parse_str(WGSL).map_err(|e| e.emit_to_string(WGSL))?;
    let info = Validator::new(ValidationFlags::all(), Capabilities::IMMEDIATES)
        .validate(&module)
        .map_err(|e| e.emit_to_string(WGSL))?;
    let mut options = spv::Options::default();
    // The shader is written for Vulkan's clip space; no y flip.
    options
        .flags
        .remove(spv::WriterFlags::ADJUST_COORDINATE_SPACE);
    spv::write_vec(&module, &info, &options, None).map_err(|e| e.to_string())
}

/// Quad size in normalized device coordinates for a `src` picture shown
/// as large as possible inside `dst` without distortion.
pub fn letterbox(src: (u32, u32), dst: (u32, u32)) -> [f32; 2] {
    crate::letterbox_size(src, dst)
}

/// Where a frame is drawn.
pub(crate) struct Target {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub format: vk::Format,
    pub extent: vk::Extent2D,
    /// Layout the image is left in (present or transfer source).
    pub final_layout: vk::ImageLayout,
}

/// Y and UV images of one picture and the descriptor set sampling them.
struct Planes {
    images: [vk::Image; 2],
    memory: [vk::DeviceMemory; 2],
    views: [vk::ImageView; 2],
    set: vk::DescriptorSet,
}

struct CpuPlanes {
    planes: Planes,
    staging: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    mapped: *mut u8,
    width: u32,
    height: u32,
}

struct Offscreen {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    readback: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    mapped: *const u8,
    width: u32,
    height: u32,
}

/// The pointer image on the GPU, with its staging buffer.
struct CursorTexture {
    serial: u32,
    width: u32,
    height: u32,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    set: vk::DescriptorSet,
    staging: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    mapped: *mut u8,
}

/// Push constants (scale, offset) placing the pointer on the video. The
/// video covers `video` (NDC half-size, centred); the pointer image of
/// `size` sits at `pos` on a screen of `screen` pixels.
pub fn cursor_rect(
    video: [f32; 2],
    pos: (i32, i32),
    size: (u32, u32),
    screen: (u32, u32),
) -> [f32; 4] {
    let (sw, sh) = (screen.0.max(1) as f32, screen.1.max(1) as f32);
    let sx = video[0] * size.0 as f32 / sw;
    let sy = video[1] * size.1 as f32 / sh;
    let left = -video[0] + 2.0 * video[0] * pos.0 as f32 / sw;
    let top = -video[1] + 2.0 * video[1] * pos.1 as f32 / sh;
    [sx, sy, left + sx, top + sy]
}

enum Source {
    None,
    Cpu,
    Foreign(u64),
}

pub struct Renderer {
    gpu: Arc<Gpu>,
    shader: vk::ShaderModule,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipelines: Vec<(vk::Format, vk::Pipeline)>,
    /// Same, for the pointer (blended on top).
    cursor_pipelines: Vec<(vk::Format, vk::Pipeline)>,
    cursor: Option<CursorTexture>,
    sampler: vk::Sampler,
    pool: vk::DescriptorPool,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    cpu: Option<CpuPlanes>,
    imported: HashMap<u64, Planes>,
    offscreen: Option<Offscreen>,
}

// SAFETY: the raw pointers are persistent mappings of memory this value
// owns; the renderer is used by one thread at a time (&mut self).
unsafe impl Send for Renderer {}

impl Renderer {
    pub fn new(gpu: Arc<Gpu>) -> Result<Self, String> {
        let words = spirv()?;
        let d = &gpu.device;
        // SAFETY: plain object creation on a valid device; Drop destroys
        // whatever was created (null handles are ignored by Vulkan).
        unsafe {
            let mut r = Self {
                shader: vk::ShaderModule::null(),
                set_layout: vk::DescriptorSetLayout::null(),
                pipeline_layout: vk::PipelineLayout::null(),
                pipelines: Vec::new(),
                cursor_pipelines: Vec::new(),
                cursor: None,
                sampler: vk::Sampler::null(),
                pool: vk::DescriptorPool::null(),
                cmd_pool: vk::CommandPool::null(),
                cmd: vk::CommandBuffer::null(),
                fence: vk::Fence::null(),
                cpu: None,
                imported: HashMap::new(),
                offscreen: None,
                gpu: gpu.clone(),
            };
            let e = |what: &'static str| move |e: vk::Result| format!("{what}: {e}");
            r.shader = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                .map_err(e("shader"))?;
            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(2)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            ];
            r.set_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .map_err(e("descriptor layout"))?;
            let push = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::VERTEX)
                .size(16)];
            let layouts = [r.set_layout];
            r.pipeline_layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&layouts)
                        .push_constant_ranges(&push),
                    None,
                )
                .map_err(e("pipeline layout"))?;
            r.sampler = d
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::LINEAR)
                        .min_filter(vk::Filter::LINEAR)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                    None,
                )
                .map_err(e("sampler"))?;
            let sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::SAMPLED_IMAGE,
                    descriptor_count: 2 * (MAX_IMPORTED as u32 + 2),
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::SAMPLER,
                    descriptor_count: MAX_IMPORTED as u32 + 2,
                },
            ];
            r.pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                        .max_sets(MAX_IMPORTED as u32 + 2)
                        .pool_sizes(&sizes),
                    None,
                )
                .map_err(e("descriptor pool"))?;
            r.cmd_pool = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                        .queue_family_index(gpu.queue_family),
                    None,
                )
                .map_err(e("command pool"))?;
            r.cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(r.cmd_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(e("command buffer"))?[0];
            r.fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(e("fence"))?;
            Ok(r)
        }
    }

    pub fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
    }

    /// The video pipeline (`cursor` false) or the pointer pipeline, which
    /// blends premultiplied alpha over the video.
    fn pipeline(&mut self, format: vk::Format, cursor: bool) -> Result<vk::Pipeline, RenderError> {
        let cache = if cursor {
            &self.cursor_pipelines
        } else {
            &self.pipelines
        };
        if let Some(&(_, p)) = cache.iter().find(|(f, _)| *f == format) {
            return Ok(p);
        }
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.shader)
                .name(c"vs"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.shader)
                .name(if cursor { c"fs_cursor" } else { c"fs" }),
        ];
        let vertex = vk::PipelineVertexInputStateCreateInfo::default();
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let mut attachment = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA);
        if cursor {
            attachment = attachment
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .src_alpha_blend_factor(vk::BlendFactor::ONE)
                .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .alpha_blend_op(vk::BlendOp::ADD);
        }
        let attachments = [attachment];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let formats = [format];
        let mut rendering =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(self.pipeline_layout)
            .push_next(&mut rendering);
        // SAFETY: all referenced state lives across the call.
        let p = unsafe {
            self.gpu
                .device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
                .map_err(|(_, e)| RenderError::Other(format!("pipeline: {e}")))?[0]
        };
        if cursor {
            self.cursor_pipelines.push((format, p));
        } else {
            self.pipelines.push((format, p));
        }
        Ok(p)
    }

    /// A descriptor set sampling `views`.
    fn descriptor_set(&self, views: [vk::ImageView; 2]) -> Result<vk::DescriptorSet, RenderError> {
        let d = &self.gpu.device;
        let layouts = [self.set_layout];
        // SAFETY: pool and layout are valid; the infos live across calls.
        unsafe {
            let set = d
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.pool)
                        .set_layouts(&layouts),
                )
                .map_err(other("descriptor set"))?[0];
            let y = [vk::DescriptorImageInfo::default()
                .image_view(views[0])
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let uv = [vk::DescriptorImageInfo::default()
                .image_view(views[1])
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let s = [vk::DescriptorImageInfo::default().sampler(self.sampler)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&y),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&uv),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&s),
            ];
            d.update_descriptor_sets(&writes, &[]);
            Ok(set)
        }
    }

    fn destroy_planes(&self, p: Planes) {
        let d = &self.gpu.device;
        // SAFETY: the GPU is idle on these (every frame waits its fence).
        unsafe {
            let _ = d.free_descriptor_sets(self.pool, &[p.set]);
            for i in 0..2 {
                d.destroy_image_view(p.views[i], None);
                d.destroy_image(p.images[i], None);
                d.free_memory(p.memory[i], None);
            }
        }
    }

    /// Copies a CPU NV12 picture into the staging buffer, (re)creating the
    /// plane images when the size changes.
    fn stage_cpu(&mut self, width: u32, height: u32, data: &[u8]) -> Result<(), RenderError> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(RenderError::Other(format!(
                "NV12 needs even sizes, got {width}×{height}"
            )));
        }
        let len = width as usize * height as usize * 3 / 2;
        if data.len() < len {
            return Err(RenderError::Other("NV12 picture too short".into()));
        }
        if self
            .cpu
            .as_ref()
            .is_none_or(|c| (c.width, c.height) != (width, height))
        {
            if let Some(old) = self.cpu.take() {
                // SAFETY: idle (fence waited), owned by us.
                unsafe {
                    let d = &self.gpu.device;
                    d.unmap_memory(old.staging_memory);
                    d.destroy_buffer(old.staging, None);
                    d.free_memory(old.staging_memory, None);
                }
                self.destroy_planes(old.planes);
            }
            let usage = vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED;
            let (y, y_mem) =
                import::device_image(&self.gpu, vk::Format::R8_UNORM, width, height, usage)
                    .map_err(RenderError::Other)?;
            let (uv, uv_mem) = import::device_image(
                &self.gpu,
                vk::Format::R8G8_UNORM,
                width / 2,
                height / 2,
                usage,
            )
            .map_err(RenderError::Other)?;
            let views = [
                import::view(&self.gpu, y, vk::Format::R8_UNORM).map_err(RenderError::Other)?,
                import::view(&self.gpu, uv, vk::Format::R8G8_UNORM).map_err(RenderError::Other)?,
            ];
            let set = self.descriptor_set(views)?;
            let (staging, staging_memory, mapped) =
                import::host_buffer(&self.gpu, len as u64, vk::BufferUsageFlags::TRANSFER_SRC)
                    .map_err(RenderError::Other)?;
            self.cpu = Some(CpuPlanes {
                planes: Planes {
                    images: [y, uv],
                    memory: [y_mem, uv_mem],
                    views,
                    set,
                },
                staging,
                staging_memory,
                mapped,
                width,
                height,
            });
        }
        let cpu = self.cpu.as_ref().expect("just created");
        // SAFETY: the mapping is len bytes, the GPU is idle on it.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), cpu.mapped, len) };
        Ok(())
    }

    fn import(&mut self, image: &fernsicht_capture::DmaBuf, key: u64) -> Result<(), RenderError> {
        if self.imported.contains_key(&key) {
            return Ok(());
        }
        if self.imported.len() >= MAX_IMPORTED {
            for (_, p) in std::mem::take(&mut self.imported) {
                self.destroy_planes(p);
            }
        }
        let [y, uv] = import::nv12(&self.gpu, image).map_err(RenderError::Import)?;
        let set = match self.descriptor_set([y.view, uv.view]) {
            Ok(set) => set,
            Err(e) => {
                import::destroy(&self.gpu, y);
                import::destroy(&self.gpu, uv);
                return Err(e);
            }
        };
        self.imported.insert(
            key,
            Planes {
                images: [y.image, uv.image],
                memory: [y.memory, uv.memory],
                views: [y.view, uv.view],
                set,
            },
        );
        Ok(())
    }

    /// Draws `picture` into `target` and waits until the GPU is done, so
    /// the picture may be reused by the decoder right after.
    pub(crate) fn render(
        &mut self,
        picture: Option<&Picture<'_>>,
        cursor: Option<&CursorOverlay>,
        target: &Target,
        wait: Option<vk::Semaphore>,
        signal: Option<vk::Semaphore>,
        after: impl FnOnce(&ash::Device, vk::CommandBuffer),
    ) -> Result<(), RenderError> {
        let (source, size) = match picture {
            None => (Source::None, (1, 1)),
            Some(Picture::Nv12 {
                width,
                height,
                data,
            }) => {
                self.stage_cpu(*width, *height, data)?;
                (Source::Cpu, (*width, *height))
            }
            Some(Picture::DmaBuf { image, key }) => {
                self.import(image, *key)?;
                (Source::Foreign(*key), (image.width, image.height))
            }
        };
        let pipeline = self.pipeline(target.format, false)?;
        // The pointer only makes sense on a picture.
        let pointer = match (cursor, &source) {
            (Some(c), Source::Cpu | Source::Foreign(_)) => {
                let upload = self.stage_cursor(c)?;
                let pipeline = self.pipeline(target.format, true)?;
                Some((c, upload, pipeline))
            }
            _ => None,
        };
        let planes = match source {
            Source::None => None,
            Source::Cpu => self.cpu.as_ref().map(|c| &c.planes),
            Source::Foreign(key) => self.imported.get(&key),
        };
        let d = &self.gpu.device;
        let cmd = self.cmd;
        let family = self.gpu.queue_family;
        let color = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barrier = |image, old, new, src_access, dst_access| {
            vk::ImageMemoryBarrier::default()
                .image(image)
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .subresource_range(color)
        };
        // SAFETY: everything recorded refers to objects owned by self or
        // the target, alive until the fence below has signalled.
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .map_err(other("reset command buffer"))?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(other("begin command buffer"))?;

            match (&source, planes) {
                (Source::Cpu, Some(p)) => {
                    let cpu = self.cpu.as_ref().expect("staged");
                    let to_dst = p.images.map(|i| {
                        barrier(
                            i,
                            vk::ImageLayout::UNDEFINED,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::AccessFlags::empty(),
                            vk::AccessFlags::TRANSFER_WRITE,
                        )
                    });
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TOP_OF_PIPE,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &to_dst,
                    );
                    let (w, h) = (cpu.width, cpu.height);
                    let layer = vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1);
                    let y = vk::BufferImageCopy::default()
                        .image_subresource(layer)
                        .image_extent(vk::Extent3D {
                            width: w,
                            height: h,
                            depth: 1,
                        });
                    let uv = vk::BufferImageCopy::default()
                        .buffer_offset(u64::from(w) * u64::from(h))
                        .image_subresource(layer)
                        .image_extent(vk::Extent3D {
                            width: w / 2,
                            height: h / 2,
                            depth: 1,
                        });
                    let dst = vk::ImageLayout::TRANSFER_DST_OPTIMAL;
                    d.cmd_copy_buffer_to_image(cmd, cpu.staging, p.images[0], dst, &[y]);
                    d.cmd_copy_buffer_to_image(cmd, cpu.staging, p.images[1], dst, &[uv]);
                    let to_read = p.images.map(|i| {
                        barrier(
                            i,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::AccessFlags::TRANSFER_WRITE,
                            vk::AccessFlags::SHADER_READ,
                        )
                    });
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::FRAGMENT_SHADER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &to_read,
                    );
                }
                (Source::Foreign(_), Some(p)) => {
                    // Take the decoder's images over from the "foreign"
                    // (VAAPI) side; GENERAL keeps their contents.
                    let acquire = p.images.map(|i| {
                        barrier(
                            i,
                            vk::ImageLayout::GENERAL,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::AccessFlags::empty(),
                            vk::AccessFlags::SHADER_READ,
                        )
                        .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                        .dst_queue_family_index(family)
                    });
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TOP_OF_PIPE,
                        vk::PipelineStageFlags::FRAGMENT_SHADER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &acquire,
                    );
                }
                _ => {}
            }

            if let Some((_, true, _)) = pointer {
                let tex = self.cursor.as_ref().expect("staged");
                let to_dst = barrier(
                    tex.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                );
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_dst],
                );
                let copy = vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: tex.width,
                        height: tex.height,
                        depth: 1,
                    });
                d.cmd_copy_buffer_to_image(
                    cmd,
                    tex.staging,
                    tex.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[copy],
                );
                let to_read = barrier(
                    tex.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::SHADER_READ,
                );
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_read],
                );
            }

            // Source stage = the stage the acquire semaphore is waited at:
            // the layout change must not start before the presentation
            // engine has released a swapchain image.
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    target.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                )],
            );
            let attachments = [vk::RenderingAttachmentInfo::default()
                .image_view(target.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: [0.0, 0.0, 0.0, 1.0],
                    },
                })];
            let area = vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: target.extent,
            };
            d.cmd_begin_rendering(
                cmd,
                &vk::RenderingInfo::default()
                    .render_area(area)
                    .layer_count(1)
                    .color_attachments(&attachments),
            );
            if let Some(p) = planes {
                d.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: target.extent.width as f32,
                        height: target.extent.height as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                d.cmd_set_scissor(cmd, 0, &[area]);
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
                d.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    &[p.set],
                    &[],
                );
                let [sx, sy] = letterbox(size, (target.extent.width, target.extent.height));
                let push: Vec<u8> = [sx, sy, 0.0f32, 0.0]
                    .iter()
                    .flat_map(|v| v.to_ne_bytes())
                    .collect();
                d.cmd_push_constants(
                    cmd,
                    self.pipeline_layout,
                    vk::ShaderStageFlags::VERTEX,
                    0,
                    &push,
                );
                d.cmd_draw(cmd, 4, 1, 0, 0);

                if let (Some((c, _, cursor_pipeline)), Some(tex)) = (pointer, &self.cursor) {
                    let rect = cursor_rect(
                        [sx, sy],
                        (c.x, c.y),
                        (tex.width, tex.height),
                        (c.screen_width, c.screen_height),
                    );
                    d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, cursor_pipeline);
                    d.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        self.pipeline_layout,
                        0,
                        &[tex.set],
                        &[],
                    );
                    let push: Vec<u8> = rect.iter().flat_map(|v| v.to_ne_bytes()).collect();
                    d.cmd_push_constants(
                        cmd,
                        self.pipeline_layout,
                        vk::ShaderStageFlags::VERTEX,
                        0,
                        &push,
                    );
                    d.cmd_draw(cmd, 4, 1, 0, 0);
                }
            }
            d.cmd_end_rendering(cmd);

            let (dst_stage, dst_access) = match target.final_layout {
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL => (
                    vk::PipelineStageFlags::TRANSFER,
                    vk::AccessFlags::TRANSFER_READ,
                ),
                _ => (
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::AccessFlags::empty(),
                ),
            };
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                dst_stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    target.image,
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    target.final_layout,
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    dst_access,
                )],
            );
            if let (Source::Foreign(_), Some(p)) = (&source, planes) {
                // Hand the images back to the decoder.
                let release = p.images.map(|i| {
                    barrier(
                        i,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::ImageLayout::GENERAL,
                        vk::AccessFlags::SHADER_READ,
                        vk::AccessFlags::empty(),
                    )
                    .src_queue_family_index(family)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                });
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &release,
                );
            }
            after(d, cmd);
            d.end_command_buffer(cmd)
                .map_err(other("end command buffer"))?;

            let cmds = [cmd];
            let waits: Vec<vk::Semaphore> = wait.into_iter().collect();
            let wait_stages = vec![vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT; waits.len()];
            let signals: Vec<vk::Semaphore> = signal.into_iter().collect();
            let submit = vk::SubmitInfo::default()
                .command_buffers(&cmds)
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&wait_stages)
                .signal_semaphores(&signals);
            {
                let _queue = self
                    .gpu
                    .queue_lock
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                d.queue_submit(self.gpu.queue, &[submit], self.fence)
                    .map_err(other("submit"))?;
            }
            let waited = d.wait_for_fences(&[self.fence], true, u64::MAX);
            d.reset_fences(&[self.fence])
                .map_err(other("reset fence"))?;
            waited.map_err(other("wait for the GPU"))?;
        }
        Ok(())
    }

    /// Renders `picture` into a `width`×`height` RGBA image and returns its
    /// pixels (tests and diagnostics).
    pub fn render_to_rgba(
        &mut self,
        picture: Option<&Picture<'_>>,
        cursor: Option<&CursorOverlay>,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, RenderError> {
        if self
            .offscreen
            .as_ref()
            .is_none_or(|o| (o.width, o.height) != (width, height))
        {
            self.drop_offscreen();
            let format = vk::Format::R8G8B8A8_UNORM;
            let (image, memory) = import::device_image(
                &self.gpu,
                format,
                width,
                height,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
            )
            .map_err(RenderError::Other)?;
            let view = import::view(&self.gpu, image, format).map_err(RenderError::Other)?;
            let (readback, readback_memory, mapped) = import::host_buffer(
                &self.gpu,
                u64::from(width) * u64::from(height) * 4,
                vk::BufferUsageFlags::TRANSFER_DST,
            )
            .map_err(RenderError::Other)?;
            self.offscreen = Some(Offscreen {
                image,
                memory,
                view,
                readback,
                readback_memory,
                mapped,
                width,
                height,
            });
        }
        let o = self.offscreen.as_ref().expect("just created");
        let (image, readback, mapped) = (o.image, o.readback, o.mapped);
        let target = Target {
            image,
            view: o.view,
            format: vk::Format::R8G8B8A8_UNORM,
            extent: vk::Extent2D { width, height },
            final_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        };
        self.render(picture, cursor, &target, None, None, |d, cmd| {
            let copy = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                });
            // SAFETY: recorded into the frame's command buffer.
            unsafe {
                d.cmd_copy_image_to_buffer(
                    cmd,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    readback,
                    &[copy],
                );
                // Make the copy visible to the CPU reading the mapping.
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::HOST,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[vk::BufferMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::HOST_READ)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .buffer(readback)
                        .size(vk::WHOLE_SIZE)],
                    &[],
                );
            };
        })?;
        let len = width as usize * height as usize * 4;
        // SAFETY: the mapping holds len bytes, written by the finished copy.
        Ok(unsafe { std::slice::from_raw_parts(mapped, len) }.to_vec())
    }

    fn drop_offscreen(&mut self) {
        if let Some(o) = self.offscreen.take() {
            let d = &self.gpu.device;
            // SAFETY: idle, owned by us.
            unsafe {
                d.destroy_image_view(o.view, None);
                d.destroy_image(o.image, None);
                d.free_memory(o.memory, None);
                d.unmap_memory(o.readback_memory);
                d.destroy_buffer(o.readback, None);
                d.free_memory(o.readback_memory, None);
            }
        }
    }

    /// Makes `c`'s image the pointer texture; `true` when it has to be
    /// copied to the GPU in this frame.
    fn stage_cursor(&mut self, c: &CursorOverlay) -> Result<bool, RenderError> {
        let img = &c.image;
        let (w, h) = (img.width, img.height);
        let len = w as usize * h as usize * 4;
        if w == 0 || h == 0 || img.pixels.len() < len {
            return Err(RenderError::Other("broken pointer image".into()));
        }
        if self.cursor.as_ref().is_some_and(|t| t.serial == c.serial) {
            return Ok(false);
        }
        if self
            .cursor
            .as_ref()
            .is_none_or(|t| (t.width, t.height) != (w, h))
        {
            self.drop_cursor();
            // BGRA in memory = DRM ARGB8888; sampled as RGBA by the shader.
            let format = vk::Format::B8G8R8A8_UNORM;
            let (image, memory) = import::device_image(
                &self.gpu,
                format,
                w,
                h,
                vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            )
            .map_err(RenderError::Other)?;
            let view = import::view(&self.gpu, image, format).map_err(RenderError::Other)?;
            let set = self.descriptor_set([view, view])?;
            let (staging, staging_memory, mapped) =
                import::host_buffer(&self.gpu, len as u64, vk::BufferUsageFlags::TRANSFER_SRC)
                    .map_err(RenderError::Other)?;
            self.cursor = Some(CursorTexture {
                serial: 0,
                width: w,
                height: h,
                image,
                memory,
                view,
                set,
                staging,
                staging_memory,
                mapped,
            });
        }
        let tex = self.cursor.as_mut().expect("just created");
        // SAFETY: the mapping holds len bytes; the GPU is idle on it.
        unsafe { std::ptr::copy_nonoverlapping(img.pixels.as_ptr(), tex.mapped, len) };
        tex.serial = c.serial;
        Ok(true)
    }

    fn drop_cursor(&mut self) {
        if let Some(t) = self.cursor.take() {
            let d = &self.gpu.device;
            // SAFETY: idle (every frame waits), owned by us.
            unsafe {
                let _ = d.free_descriptor_sets(self.pool, &[t.set]);
                d.destroy_image_view(t.view, None);
                d.destroy_image(t.image, None);
                d.free_memory(t.memory, None);
                d.unmap_memory(t.staging_memory);
                d.destroy_buffer(t.staging, None);
                d.free_memory(t.staging_memory, None);
            }
        }
    }

    /// Forgets imported decoder surfaces (the decoder was replaced).
    pub fn forget_imports(&mut self) {
        for (_, p) in std::mem::take(&mut self.imported) {
            self.destroy_planes(p);
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // Wait for the GPU, then destroy what we own.
        self.gpu.wait_idle();
        self.forget_imports();
        self.drop_offscreen();
        self.drop_cursor();
        if let Some(c) = self.cpu.take() {
            // SAFETY: idle, owned by us.
            unsafe {
                let d = &self.gpu.device;
                d.unmap_memory(c.staging_memory);
                d.destroy_buffer(c.staging, None);
                d.free_memory(c.staging_memory, None);
            }
            self.destroy_planes(c.planes);
        }
        let d = &self.gpu.device;
        // SAFETY: as above.
        unsafe {
            for (_, p) in self
                .pipelines
                .drain(..)
                .chain(self.cursor_pipelines.drain(..))
            {
                d.destroy_pipeline(p, None);
            }
            d.destroy_fence(self.fence, None);
            d.destroy_command_pool(self.cmd_pool, None);
            d.destroy_descriptor_pool(self.pool, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_pipeline_layout(self.pipeline_layout, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_shader_module(self.shader, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shader_compiles_to_spirv() {
        let words = spirv().unwrap();
        assert_eq!(words[0], 0x0723_0203, "SPIR-V magic");
    }

    #[test]
    fn pointer_placement_follows_the_scaled_video() {
        // Full-size video, screen = video: a 64×64 pointer at the top-left
        // corner of a 640×480 screen spans x -1..-0.8, y -1..-0.733.
        let [sx, sy, ox, oy] = cursor_rect([1.0, 1.0], (0, 0), (64, 64), (640, 480));
        assert!((sx - 0.1).abs() < 1e-6 && (sy - 64.0 / 480.0).abs() < 1e-6);
        assert!((ox - sx + 1.0).abs() < 1e-6 && (oy - sy + 1.0).abs() < 1e-6);
        // Letterboxed video (half the height): the pointer shrinks with it.
        let [_, sy2, _, oy2] = cursor_rect([1.0, 0.5], (0, 240), (64, 64), (640, 480));
        assert!((sy2 - sy / 2.0).abs() < 1e-6);
        assert!((oy2 - sy2).abs() < 1e-6, "screen middle is NDC 0");
    }

    #[test]
    fn letterbox_keeps_the_aspect_ratio() {
        assert_eq!(letterbox((1920, 1080), (1920, 1080)), [1.0, 1.0]);
        assert_eq!(letterbox((1920, 1080), (960, 540)), [1.0, 1.0]);
        // 16:9 in a square: full width, bars top and bottom.
        let [x, y] = letterbox((1600, 900), (1000, 1000));
        assert_eq!(x, 1.0);
        assert!((y - 0.5625).abs() < 1e-6);
        // 4:3 in 16:9: full height, bars left and right.
        let [x, y] = letterbox((1440, 1080), (1920, 1080));
        assert!((x - 0.75).abs() < 1e-6);
        assert_eq!(y, 1.0);
        // Degenerate sizes do not divide by zero.
        assert!(letterbox((0, 0), (0, 0)).iter().all(|v| v.is_finite()));
    }
}
