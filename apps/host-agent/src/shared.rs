//! What the control loop shares with a session's pipeline threads, and
//! the counters across sessions.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fernsicht_net::keyframe;
use fernsicht_net::keyframe::Sent;

/// State the control loop shares with the pipeline threads.
/// Cumulative counters across all sessions of one host agent.
#[derive(Debug, Default)]
pub struct HostStats {
    pub sessions: AtomicU64,
    pub frames_captured: AtomicU64,
    /// Captured frames not encoded: the screen had not changed.
    pub frames_still: AtomicU64,
    pub frames_encoded: AtomicU64,
    pub keyframes_encoded: AtomicU64,
    /// Encoded frames dropped because the sender was a whole queue behind.
    pub send_overflows: AtomicU64,
    pub frames_sent: AtomicU64,
    pub audio_frames_sent: AtomicU64,
}

impl HostStats {
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

pub(crate) struct Shared {
    pub(crate) stats: Arc<HostStats>,
    pub(crate) running: AtomicBool,
    pub(crate) keyframe_requested: AtomicBool,
    /// The last keyframe encoded: requests it answers are skipped.
    pub(crate) last_keyframe: Mutex<Option<Sent>>,
    /// One frame at the session's rate.
    pub(crate) frame: Duration,
    /// The client lacks the pointer image.
    pub(crate) cursor_requested: AtomicBool,
    /// Loss rate FEC is sized for, as `f32` bits.
    pub(crate) fec_loss: AtomicU32,
    /// The bitrate the encoder should use now, kbit/s.
    pub(crate) target_kbps: AtomicU32,
    /// The client asked for another monitor (index in the list it got).
    pub(crate) monitor_request: std::sync::Mutex<Option<u8>>,
    /// The monitor shown and the others; changes count `screen_changes`.
    pub(crate) screen: std::sync::Mutex<Option<fernsicht_capture::ScreenInfo>>,
    pub(crate) screen_changes: AtomicU64,
}

impl Shared {
    /// The client asked for a keyframe: one comes, unless one is on its way
    /// already (see [`keyframe::already_coming`]). `client_has` is the last
    /// frame the client completed, `rtt` its round trip.
    pub(crate) fn want_keyframe(&self, client_has: Option<u32>, rtt: Duration) {
        let last = *self.last_keyframe.lock().unwrap_or_else(|e| e.into_inner());
        let window = keyframe::in_flight_for(rtt, self.frame);
        if keyframe::already_coming(last, client_has, Instant::now(), window) {
            log::trace!("keyframe request: one is on its way");
        } else {
            self.keyframe_requested.store(true, Ordering::Relaxed);
        }
    }

    /// Keyframe `frame_id` went to the sender.
    pub(crate) fn sent_keyframe(&self, frame_id: u32) {
        *self.last_keyframe.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sent {
            frame_id,
            at: Instant::now(),
        });
    }

    /// Asks the capture thread to show monitor `index`.
    pub(crate) fn select_monitor(&self, index: u8) {
        *self
            .monitor_request
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(index);
    }

    pub(crate) fn screen(&self) -> Option<fernsicht_capture::ScreenInfo> {
        self.screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn set_screen(&self, screen: Option<fernsicht_capture::ScreenInfo>) {
        *self.screen.lock().unwrap_or_else(|e| e.into_inner()) = screen;
        self.screen_changes.fetch_add(1, Ordering::Release);
    }
}
