//! The capture thread: frames from the source to the encoder's slot, the
//! pointer after them, and switching monitors.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use fernsicht_capture::{Frame, FrameSource};
use fernsicht_core::Slot;

use crate::health::RepeatedError;
use crate::monitors::{MONITORS_REPEAT, MonitorSender};
use crate::pointer::CursorSender;
use crate::shared::{HostStats, Shared};

pub(crate) fn capture_loop(
    mut source: Box<dyn FrameSource>,
    shared: &Shared,
    slot: &Slot<Frame>,
    free_rx: &Receiver<Frame>,
    free_tx: &Sender<Frame>,
    mut cursor: CursorSender,
    mut monitors: MonitorSender,
) {
    let (mut failures, mut errors) = (0u32, RepeatedError::default());
    let mut rescanned = Instant::now();
    let (mut last_change, mut last_encoded) = (Instant::now(), Instant::now());
    while shared.running.load(Ordering::Acquire) {
        // Monitors plugged in or out: the client's list and the input
        // mapping follow.
        if rescanned.elapsed() >= MONITORS_REPEAT {
            rescanned = Instant::now();
            source.rescan();
            let screen = source.screen();
            if screen != shared.screen() {
                shared.set_screen(screen);
            }
        }
        let request = shared
            .monitor_request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(index) = request {
            switch_monitor(source.as_mut(), shared, &mut cursor, index);
        }
        monitors.tick(shared);
        let Ok(mut frame) = free_rx.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        if let Err(e) = source.next_frame(&mut frame) {
            // Mostly passing (a monitor in standby, a framebuffer swapped
            // during a mode change): keep the session, try again.
            if let Some(line) = errors.report(e.to_string(), Instant::now()) {
                log::warn!("capture: {line}");
            }
            let _ = free_tx.send(frame);
            std::thread::sleep(crate::health::capture_retry_delay(failures));
            failures = failures.saturating_add(1);
            continue;
        }
        if failures > 0 {
            log::info!("capture: back after {failures} failed attempts");
            failures = 0;
        }
        HostStats::bump(&shared.stats.frames_captured);
        let now = Instant::now();
        if frame.changed {
            last_change = now;
        }
        let wanted = shared.keyframe_requested.load(Ordering::Relaxed);
        if crate::still::should_encode(frame.changed, wanted, now - last_change, now - last_encoded)
        {
            last_encoded = now;
            // The frame first: the encoder starts while the pointer is read.
            if let Some(skipped) = slot.put(frame) {
                let _ = free_tx.send(skipped);
            }
        } else {
            // Nothing new on the screen: the encoder rests.
            HostStats::bump(&shared.stats.frames_still);
            let _ = free_tx.send(frame);
        }
        // Then the pointer, right away, not after encoding: it is the one
        // thing the client can show before the frame it belongs to.
        if let Some(c) = source.cursor() {
            let asked = shared.cursor_requested.swap(false, Ordering::Relaxed);
            cursor.send(&c, asked);
        }
    }
    slot.close();
}

/// Shows monitor `index` of the list the client got; the old one stays if
/// that fails.
pub(crate) fn switch_monitor(
    source: &mut dyn FrameSource,
    shared: &Shared,
    cursor: &mut CursorSender,
    index: u8,
) {
    let Some(name) = shared
        .screen()
        .and_then(|s| s.active.get(usize::from(index)).cloned())
    else {
        log::info!("monitor {index} asked for, but there is none");
        return;
    };
    if shared.screen().is_some_and(|s| s.connector == name) {
        return;
    }
    match source.switch_to(&name) {
        Ok(()) => {
            shared.set_screen(source.screen());
            cursor.set_screen((source.width(), source.height()));
            // The decoder needs a fresh start for the new picture.
            shared.keyframe_requested.store(true, Ordering::Relaxed);
        }
        Err(e) => log::warn!("switching to {name}: {e}"),
    }
}
