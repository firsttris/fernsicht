//! A running session: its state on the control loop, feedback from the
//! client, and saying goodbye.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fernsicht_capture::Frame;
use fernsicht_core::Slot;
use fernsicht_input::{Dedup, InputSink};
use fernsicht_net::{LossEstimator, RateController};
use fernsicht_proto::{Bye, Codec, Feedback, InputHeader};
use fernsicht_secure::PublicKey;

use crate::config::SessionParams;
use crate::health::RepeatedError;
use crate::link::Link;
use crate::shared::{HostStats, Shared};

pub(crate) struct Session {
    pub(crate) params: SessionParams,
    pub(crate) codec: Codec,
    /// The paired device's key (secure sessions): unpairing it ends the
    /// session.
    pub(crate) client: Option<PublicKey>,
    /// The client's address (for a browser: its HTTP address; its packets
    /// never come to our socket).
    pub(crate) peer: SocketAddr,
    /// A browser over WebRTC: it ends itself when the browser goes.
    pub(crate) web: bool,
    /// Sends to the client (sealed for secure sessions).
    pub(crate) link: Link,
    /// The handshake that started it (first message and our answer), to
    /// answer a retransmission the same.
    pub(crate) handshake: Option<(Vec<u8>, Vec<u8>)>,
    pub(crate) last_seen: Instant,
    pub(crate) shared: Arc<Shared>,
    pub(crate) loss: LossEstimator,
    /// Adapts the bitrate to the loss the client reports.
    pub(crate) rate: RateController,
    /// Sender overflows counted at the last report.
    pub(crate) overflows_seen: u64,
    /// The round trip the client last reported (with keyframe requests).
    pub(crate) client_rtt: Duration,
    pub(crate) threads: Vec<JoinHandle<()>>,
    pub(crate) frame_slot: Arc<Slot<Frame>>,
    /// Released (all keys up) when the session ends.
    pub(crate) input: Option<Box<dyn InputSink>>,
    pub(crate) input_seen: Dedup,
    pub(crate) input_errors: RepeatedError,
    /// GPU clocked up while the session runs; dropped with it.
    pub(crate) _gpu: Option<crate::power::GpuBoost>,
}

impl Session {
    /// Applies the input events not applied yet, in order.
    pub(crate) fn apply_input(&mut self, body: &[u8]) {
        for (seq, event) in InputHeader::events(body) {
            if !self.input_seen.accept(seq) {
                continue;
            }
            if let Some(sink) = self.input.as_mut()
                && let Err(e) = sink.inject(&event)
                && let Some(msg) = self.input_errors.report(e, Instant::now())
            {
                log::warn!("input: {msg}");
            }
        }
    }

    pub(crate) fn stop(mut self) {
        if let Some(mut input) = self.input.take() {
            input.release_all();
        }
        self.shared.running.store(false, Ordering::Release);
        self.frame_slot.close();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Tells a native client its session is over (a browser has its own way).
pub(crate) fn say_bye(s: &Session, out: &mut [u8]) {
    if s.web {
        return;
    }
    let n = Bye {
        session_id: s.params.session_id,
    }
    .encode(out);
    let _ = s.link.send(&out[..n]);
}

pub(crate) fn log_session(s: &Session, who: &str) {
    let p = s.params;
    log::info!(
        "session {:08x}: {who} {}x{}@{} {} kbit/s{}",
        p.session_id,
        p.width,
        p.height,
        p.fps,
        p.bitrate_kbps,
        if s.link.encrypted() {
            ", encrypted"
        } else {
            ""
        }
    );
}

pub(crate) fn on_feedback(s: &mut Session, fb: &Feedback) {
    if fb.request_keyframe {
        // Its frame id is the highest seen, not the last complete: the
        // round trip alone tells.
        s.shared.want_keyframe(None, s.client_rtt);
    }
    if fb.request_cursor {
        s.shared.cursor_requested.store(true, Ordering::Relaxed);
    }
    // An interval without packets (a still screen, a monitor in standby)
    // says nothing about the network.
    if u64::from(fb.packets_received) + u64::from(fb.packets_lost) == 0 {
        return;
    }
    let estimate = s.loss.update(fb.loss_ratio());
    s.shared
        .fec_loss
        .store(estimate.to_bits(), Ordering::Relaxed);
    let overflows = HostStats::get(&s.shared.stats.send_overflows);
    let overflow = overflows > s.overflows_seen;
    s.overflows_seen = overflows;
    if let Some(kbps) = s
        .rate
        .report(estimate, fb.frames_dropped, overflow, Instant::now())
    {
        log::info!(
            "session {:08x}: bitrate {:.1} Mbit/s (loss {:.1} %, {} frames lost{})",
            s.params.session_id,
            f64::from(kbps) / 1000.0,
            estimate * 100.0,
            fb.frames_dropped,
            if overflow { ", sender behind" } else { "" }
        );
        s.shared.target_kbps.store(kbps, Ordering::Relaxed);
    }
    log::debug!(
        "feedback: loss {:.2} % recovered {} dropped {} → FEC sized for {:.1} %",
        fb.loss_ratio() * 100.0,
        fb.packets_recovered,
        fb.frames_dropped,
        estimate * 100.0
    );
}

pub(crate) fn codec_label(codec: Codec) -> &'static str {
    match codec {
        Codec::Synthetic => "Synthetisch",
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
    }
}

pub(crate) fn new_session_id() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.as_nanos() as u32) ^ std::process::id().rotate_left(16) ^ 0x5EED_F00D
}
