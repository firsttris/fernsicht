//! A GPU that stops finishing work is given up instead of waited for.
//!
//! The hang is made on purpose: a submission waits for a timeline
//! semaphore value nobody signals, which is what a faulted GPU looks like
//! from the outside (the work never finishes). Runs on any Vulkan device,
//! llvmpipe included; without Vulkan it skips, unless
//! `FERNSICHT_REQUIRE_VULKAN` is set.

use std::time::{Duration, Instant};

use ash::vk;
use fernsicht_gpu::{GIVEN_UP, Gpu, Timeline, submit_timeline, wait_fence};

fn gpu() -> Option<Gpu> {
    match Gpu::new() {
        Ok(g) if g.features.timeline_semaphore => Some(g),
        Ok(g) => {
            eprintln!("skipped: {} has no timeline semaphores", g.name());
            None
        }
        Err(e) => {
            assert!(
                std::env::var_os("FERNSICHT_REQUIRE_VULKAN").is_none(),
                "Vulkan required: {e}"
            );
            eprintln!("skipped: {e}");
            None
        }
    }
}

/// An empty command buffer, a fence and a timeline semaphore at 0.
fn objects(gpu: &Gpu) -> (vk::CommandBuffer, vk::Fence, vk::Semaphore) {
    let d = &gpu.device;
    // SAFETY: plain object creation on a valid device; left to the
    // process's end (the GPU is given up in the test).
    unsafe {
        let pool = d
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family),
                None,
            )
            .unwrap();
        let cmd = d
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
            .unwrap()[0];
        d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
            .unwrap();
        d.end_command_buffer(cmd).unwrap();
        let fence = d
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .unwrap();
        let mut timeline = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let sem = d
            .create_semaphore(
                &vk::SemaphoreCreateInfo::default().push_next(&mut timeline),
                None,
            )
            .unwrap();
        (cmd, fence, sem)
    }
}

#[test]
fn a_gpu_that_stops_finishing_work_is_given_up_not_waited_for() {
    let Some(gpu) = gpu() else { return };
    let (cmd, fence, sem) = objects(&gpu);
    // Waits for value 1, which never comes.
    let stuck = Timeline {
        semaphore: sem,
        wait: 1,
        signal: 2,
    };
    // SAFETY: our objects, on the device.
    unsafe { submit_timeline(&gpu, &[cmd], None, None, Some(stuck), fence) }.unwrap();
    assert!(!gpu.is_lost());

    let t = Instant::now();
    // SAFETY: the fence was submitted.
    let waited = unsafe { wait_fence(&gpu, fence) };
    let took = t.elapsed();
    assert!(waited.unwrap_err().contains(GIVEN_UP));
    assert!(
        took < gpu.timeout + Duration::from_secs(2),
        "gave up after {took:?}"
    );
    assert!(gpu.is_lost());

    // Nothing waits for it any more, and new work fails at once.
    let t = Instant::now();
    gpu.wait_idle();
    // SAFETY: as above.
    let again = unsafe { submit_timeline(&gpu, &[cmd], None, None, None, fence) };
    assert_eq!(again.unwrap_err(), GIVEN_UP);
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "waited {:?}",
        t.elapsed()
    );

    // The test's own hang is ended, so nothing stuck is left on a real
    // GPU: NVIDIA's driver waited for it when the process exited.
    // SAFETY: our semaphore; value 1 is what the work waits for.
    unsafe {
        gpu.device
            .signal_semaphore(&vk::SemaphoreSignalInfo::default().semaphore(sem).value(1))
            .unwrap();
        gpu.device
            .wait_for_fences(&[fence], true, gpu.timeout.as_nanos() as u64)
            .unwrap();
    }
    // A GPU given up is left to the process's end, not waited for.
    drop(gpu);
}
