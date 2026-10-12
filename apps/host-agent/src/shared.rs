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
    /// Video packets sent again on a NACK.
    pub packets_resent: AtomicU64,
    /// Packets the client reported marked CE (ECN) on the way.
    pub packets_ce: AtomicU64,
    /// Times the rate controller lowered the bitrate, and the bitrate it
    /// set last (kbit/s; 0 before it changed one).
    pub bitrate_cuts: AtomicU64,
    pub bitrate_kbps: AtomicU64,
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
    /// What the client last measured the path deliver, kbit/s (0: not
    /// yet): the sender paces at it (C3).
    pub(crate) delivery_kbps: AtomicU32,
    /// The client's NACKs, for the sender to answer (C2).
    pub(crate) nacks: (
        crossbeam_channel::Sender<fernsicht_proto::Nack>,
        crossbeam_channel::Receiver<fernsicht_proto::Nack>,
    ),
    /// The client asked for another monitor (index in the list it got).
    pub(crate) monitor_request: std::sync::Mutex<Option<u8>>,
    /// The monitor shown and the others; changes count `screen_changes`.
    pub(crate) screen: std::sync::Mutex<Option<fernsicht_capture::ScreenInfo>>,
    pub(crate) screen_changes: AtomicU64,
    /// Where input last put the pointer, 0..65535 on the monitor shown, and
    /// when (packed, see [`Shared::note_pointer`]); `NO_POINTER` before.
    pub(crate) pointer: AtomicU64,
    /// When the encode thread's current encoder call began (µs, 0: none
    /// running): the control loop ends a session whose encoder hangs.
    pub(crate) encoding_since: AtomicU64,
}

/// [`Shared::pointer`] before any absolute pointer input.
pub(crate) const NO_POINTER: u64 = u64::MAX;

impl Shared {
    /// Input put the pointer at `(x, y)` (0..65535 on the monitor shown).
    pub(crate) fn note_pointer(&self, x: u16, y: u16) {
        let ms = (fernsicht_core::now_us() / 1000) as u32;
        let packed = u64::from(x) | u64::from(y) << 16 | u64::from(ms) << 32;
        self.pointer.store(packed, Ordering::Relaxed);
    }

    /// Where input last put the pointer, and how many ms ago.
    pub(crate) fn pointer(&self) -> Option<((u16, u16), u32)> {
        let p = self.pointer.load(Ordering::Relaxed);
        (p != NO_POINTER).then(|| {
            let now = (fernsicht_core::now_us() / 1000) as u32;
            (
                (p as u16, (p >> 16) as u16),
                now.wrapping_sub((p >> 32) as u32),
            )
        })
    }

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
