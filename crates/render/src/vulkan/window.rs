//! The video window: the renderer drawing into a swapchain.
//!
//! Present mode Mailbox (newest frame replaces a waiting one, no tearing),
//! else Immediate (may tear), else FIFO (vsync, adds up to a refresh of
//! latency). The window is created by the caller on the main thread
//! (winit's rule); presenting happens on the client's present thread.

use std::sync::Arc;

use ash::vk;
use fernsicht_codec::{DecodedFrame, Picture, PictureKind};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::Window;

use super::renderer::Target;
use super::{Gpu, RenderError, Renderer};
use crate::{CursorOverlay, Presenter};

/// Present modes from most to least preferred for latency.
pub fn pick_present_mode(available: &[vk::PresentModeKHR]) -> vk::PresentModeKHR {
    [vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::IMMEDIATE]
        .into_iter()
        .find(|m| available.contains(m))
        .unwrap_or(vk::PresentModeKHR::FIFO)
}

/// An 8-bit UNORM format: the shader writes display values directly.
pub fn pick_format(available: &[vk::SurfaceFormatKHR]) -> Option<vk::SurfaceFormatKHR> {
    [vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM]
        .into_iter()
        .find_map(|f| {
            available
                .iter()
                .find(|s| s.format == f && s.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
                .copied()
        })
}

struct Swapchain {
    handle: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    format: vk::Format,
    extent: vk::Extent2D,
    /// Signalled by acquire; one is enough because every frame waits for
    /// the GPU before the next acquire.
    acquired: vk::Semaphore,
    /// Per image: rendering done, the presentation engine may show it.
    rendered: Vec<vk::Semaphore>,
}

pub struct WindowPresenter {
    window: Arc<Window>,
    renderer: Renderer,
    surface_fn: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    swapchain: Option<Swapchain>,
    mode: vk::PresentModeKHR,
    wants: PictureKind,
    title: String,
    pub presented: u64,
}

// SAFETY: Vulkan handles are plain ids, the window is Send + Sync; the
// presenter is used by one thread (&mut self).
unsafe impl Send for WindowPresenter {}

impl WindowPresenter {
    pub fn new(window: Arc<Window>, title: &str) -> Result<Self, String> {
        let display = window
            .display_handle()
            .map_err(|e| format!("display handle: {e}"))?
            .as_raw();
        let handle = window
            .window_handle()
            .map_err(|e| format!("window handle: {e}"))?
            .as_raw();
        let extensions = ash_window::enumerate_required_extensions(display)
            .map_err(|e| format!("surface extensions: {e}"))?;
        let gpu = Arc::new(Gpu::create(extensions, true)?);
        // SAFETY: the window outlives the surface (we hold an Arc).
        let surface = unsafe {
            ash_window::create_surface(&gpu.entry, &gpu.instance, display, handle, None)
                .map_err(|e| format!("create surface: {e}"))?
        };
        let surface_fn = ash::khr::surface::Instance::new(&gpu.entry, &gpu.instance);
        // SAFETY: valid physical device and surface.
        let supported = unsafe {
            surface_fn
                .get_physical_device_surface_support(gpu.physical, gpu.queue_family, surface)
                .unwrap_or(false)
        };
        if !supported {
            // SAFETY: created above.
            unsafe { surface_fn.destroy_surface(surface, None) };
            return Err(format!("{} cannot present to this window", gpu.name()));
        }
        let renderer = match Renderer::new(gpu.clone()) {
            Ok(r) => r,
            Err(e) => {
                // SAFETY: created above.
                unsafe { surface_fn.destroy_surface(surface, None) };
                return Err(e);
            }
        };
        log::info!(
            "window on {}{}",
            gpu.name(),
            if gpu.can_import_dmabuf() {
                ", decoded pictures shown without a copy"
            } else {
                ", no DMA-BUF import: pictures go through the CPU"
            }
        );
        Ok(Self {
            wants: if gpu.can_import_dmabuf() {
                PictureKind::DmaBuf
            } else {
                PictureKind::Nv12
            },
            window,
            renderer,
            surface_fn,
            surface,
            swapchain: None,
            mode: vk::PresentModeKHR::FIFO,
            title: title.to_owned(),
            presented: 0,
        })
    }

    fn destroy_swapchain(&mut self) {
        let Some(sc) = self.swapchain.take() else {
            return;
        };
        let gpu = self.renderer.gpu();
        let swapchain_fn = gpu.swapchain.as_ref().expect("created for a window");
        gpu.wait_idle();
        // SAFETY: the GPU is idle; owned by us.
        unsafe {
            for v in sc.views {
                gpu.device.destroy_image_view(v, None);
            }
            for s in sc.rendered {
                gpu.device.destroy_semaphore(s, None);
            }
            gpu.device.destroy_semaphore(sc.acquired, None);
            swapchain_fn.destroy_swapchain(sc.handle, None);
        }
    }

    /// (Re)creates the swapchain for the window's current size. `false`
    /// when the window has no area (minimized).
    fn ensure_swapchain(&mut self) -> Result<bool, String> {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return Ok(false);
        }
        if self
            .swapchain
            .as_ref()
            .is_some_and(|s| (s.extent.width, s.extent.height) == (size.width, size.height))
        {
            return Ok(true);
        }
        let old = self
            .swapchain
            .as_ref()
            .map_or(vk::SwapchainKHR::null(), |s| s.handle);
        let gpu = self.renderer.gpu().clone();
        let swapchain_fn = gpu.swapchain.as_ref().expect("created for a window");
        let e = |what: &'static str| move |e: vk::Result| format!("{what}: {e}");
        // SAFETY: surface and device are valid; new objects are owned by the
        // new Swapchain value, the old swapchain is retired below.
        unsafe {
            let caps = self
                .surface_fn
                .get_physical_device_surface_capabilities(gpu.physical, self.surface)
                .map_err(e("surface capabilities"))?;
            let formats = self
                .surface_fn
                .get_physical_device_surface_formats(gpu.physical, self.surface)
                .map_err(e("surface formats"))?;
            let modes = self
                .surface_fn
                .get_physical_device_surface_present_modes(gpu.physical, self.surface)
                .map_err(e("present modes"))?;
            let format = pick_format(&formats).ok_or("no 8-bit UNORM surface format")?;
            self.mode = pick_present_mode(&modes);
            let extent = if caps.current_extent.width != u32::MAX {
                caps.current_extent
            } else {
                vk::Extent2D {
                    width: size
                        .width
                        .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                    height: size
                        .height
                        .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
                }
            };
            // Mailbox needs a spare image to replace; keep the count minimal
            // otherwise, every queued image is latency.
            let mut count = caps.min_image_count.max(2);
            if self.mode == vk::PresentModeKHR::MAILBOX {
                count = count.max(3);
            }
            if caps.max_image_count > 0 {
                count = count.min(caps.max_image_count);
            }
            let handle = swapchain_fn
                .create_swapchain(
                    &vk::SwapchainCreateInfoKHR::default()
                        .surface(self.surface)
                        .min_image_count(count)
                        .image_format(format.format)
                        .image_color_space(format.color_space)
                        .image_extent(extent)
                        .image_array_layers(1)
                        .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .pre_transform(caps.current_transform)
                        .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                        .present_mode(self.mode)
                        .clipped(true)
                        .old_swapchain(old),
                    None,
                )
                .map_err(e("create swapchain"))?;
            self.destroy_swapchain();
            let images = swapchain_fn
                .get_swapchain_images(handle)
                .map_err(e("swapchain images"))?;
            let mut views = Vec::new();
            let mut rendered = Vec::new();
            for &image in &images {
                views.push(super::import::view(&gpu, image, format.format)?);
                rendered.push(
                    gpu.device
                        .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                        .map_err(e("semaphore"))?,
                );
            }
            let acquired = gpu
                .device
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                .map_err(e("semaphore"))?;
            log::info!(
                "swapchain {}×{}, {} images, {:?}",
                extent.width,
                extent.height,
                images.len(),
                self.mode
            );
            self.swapchain = Some(Swapchain {
                handle,
                images,
                views,
                format: format.format,
                extent,
                acquired,
                rendered,
            });
        }
        Ok(true)
    }

    fn draw(
        &mut self,
        picture: Option<&Picture<'_>>,
        cursor: Option<&CursorOverlay>,
    ) -> Result<(), RenderError> {
        let ensure = self.ensure_swapchain().map_err(RenderError::Other)?;
        if !ensure {
            return Ok(());
        }
        let gpu = self.renderer.gpu().clone();
        let swapchain_fn = gpu.swapchain.as_ref().expect("created for a window");
        let sc = self.swapchain.as_ref().expect("ensured");
        // SAFETY: valid swapchain; the semaphore is unsignalled because the
        // previous frame was waited for.
        let acquired = unsafe {
            swapchain_fn.acquire_next_image(sc.handle, u64::MAX, sc.acquired, vk::Fence::null())
        };
        let index = match acquired {
            Ok((i, _suboptimal)) => i as usize,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                // Resized under us: drop this frame, rebuild next time.
                self.destroy_swapchain();
                return Ok(());
            }
            Err(e) => return Err(RenderError::Other(format!("acquire: {e}"))),
        };
        let target = Target {
            image: sc.images[index],
            view: sc.views[index],
            format: sc.format,
            extent: sc.extent,
            final_layout: vk::ImageLayout::PRESENT_SRC_KHR,
        };
        let (handle, wait, done) = (sc.handle, sc.acquired, sc.rendered[index]);
        self.renderer
            .render(picture, cursor, &target, Some(wait), Some(done), |_, _| {})?;
        let waits = [done];
        let swapchains = [handle];
        let indices = [index as u32];
        // SAFETY: the image was acquired and rendered above.
        let presented = unsafe {
            let _queue = gpu.queue_lock.lock().unwrap_or_else(|e| e.into_inner());
            swapchain_fn.queue_present(
                gpu.queue,
                &vk::PresentInfoKHR::default()
                    .wait_semaphores(&waits)
                    .swapchains(&swapchains)
                    .image_indices(&indices),
            )
        };
        match presented {
            Ok(false) => {}
            Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.destroy_swapchain(),
            Err(e) => return Err(RenderError::Other(format!("present: {e}"))),
        }
        Ok(())
    }
}

impl Presenter for WindowPresenter {
    fn wants(&self) -> Option<PictureKind> {
        Some(self.wants)
    }

    fn present(
        &mut self,
        _frame: &DecodedFrame,
        picture: Option<&Picture<'_>>,
        cursor: Option<&CursorOverlay>,
    ) -> Result<(), String> {
        match self.draw(picture, cursor) {
            Ok(()) => {
                self.presented += 1;
                Ok(())
            }
            Err(RenderError::Import(e)) if self.wants == PictureKind::DmaBuf => {
                // Not this buffer layout; from now on through the CPU.
                log::warn!("showing decoded pictures without a copy failed ({e}); using the CPU");
                self.wants = PictureKind::Nv12;
                self.renderer.forget_imports();
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn overlay(&mut self, lines: &[String]) {
        // The first line is the glass-to-glass summary; it fits a title.
        if let Some(first) = lines.first() {
            self.window.set_title(&format!("{} · {first}", self.title));
        }
    }
}

impl Drop for WindowPresenter {
    fn drop(&mut self) {
        self.destroy_swapchain();
        // SAFETY: the swapchain is gone, the surface is ours.
        unsafe { self.surface_fn.destroy_surface(self.surface, None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowest_latency_present_mode_wins() {
        use vk::PresentModeKHR as M;
        assert_eq!(
            pick_present_mode(&[M::FIFO, M::IMMEDIATE, M::MAILBOX]),
            M::MAILBOX
        );
        assert_eq!(pick_present_mode(&[M::FIFO, M::IMMEDIATE]), M::IMMEDIATE);
        assert_eq!(pick_present_mode(&[M::FIFO]), M::FIFO);
        assert_eq!(pick_present_mode(&[]), M::FIFO);
    }

    #[test]
    fn unorm_formats_only() {
        let f = |format| vk::SurfaceFormatKHR {
            format,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        };
        let srgb_only = [f(vk::Format::B8G8R8A8_SRGB)];
        assert_eq!(pick_format(&srgb_only), None);
        let both = [f(vk::Format::B8G8R8A8_SRGB), f(vk::Format::R8G8B8A8_UNORM)];
        assert_eq!(
            pick_format(&both).unwrap().format,
            vk::Format::R8G8B8A8_UNORM
        );
    }
}
