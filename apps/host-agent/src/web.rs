//! The web viewer: the host serves the viewer page and runs WebRTC
//! sessions for browsers.
//!
//! ```text
//! browser ── GET /            ──► viewer page (web/viewer, built)
//!         ── GET /api/info    ──► host name, pairing open?
//!         ── POST /api/connect {pin, offer} ──► host loop: PIN, session ──► {answer}
//!         ◄═ WebRTC (UDP) ═══ H.264 video, Opus sound, data channel:
//!                              pointer (proto packets) and stats → browser,
//!                              mouse and keyboard (JSON) → host
//! ```
//!
//! Browsers reach the page over plain `http://` in the LAN, where only
//! WebRTC of the browser media APIs works (WebCodecs and WebTransport need
//! HTTPS). Picture, sound and input are encrypted by WebRTC (DTLS-SRTP).
//! A browser is let in with the PIN the host shows for pairing; it is
//! used once and not remembered. The PIN itself crosses the LAN in the
//! clear, like the page.
//!
//! [`server`] answers the page's requests, [`offer`] answers a browser's
//! offer, [`rtc`] runs the WebRTC thread and [`commands`] reads what the
//! browser sends over the data channel.

mod commands;
mod offer;
mod rtc;
mod server;

pub(crate) use offer::{WebSetup, accept};
pub(crate) use rtc::spawn;
pub(crate) use server::{check_pin, serve};

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};
use fernsicht_codec::EncodedFrame;
use serde_json::json;
use str0m::change::{SdpAnswer, SdpOffer};

use crate::shared::Shared;
use crate::{HostStats, SessionParams};

/// What a session sends to the browser, from its threads to the WebRTC
/// thread.
pub(crate) enum WebOut {
    /// A proto packet (pointer) for the data channel.
    Packet(Vec<u8>),
    Video {
        data: Vec<u8>,
        capture_us: u64,
    },
    /// One Opus frame of [`fernsicht_audio::FRAME_SAMPLES`] samples.
    Audio {
        data: Vec<u8>,
    },
    /// Text for the data channel (JSON).
    Text(String),
}

/// Messages queued for the WebRTC thread before the sender waits.
const OUT_QUEUE: usize = 64;

/// A browser that has not connected by then never will (its packets are
/// blocked, or the tab was closed while connecting): the session ends
/// instead of encoding for nobody.
pub(crate) const CONNECT_WITHIN: Duration = Duration::from_secs(10);

/// A browser asking for a session, from the HTTP thread to the host's loop.
pub(crate) struct WebRequest {
    pub pin: String,
    pub offer: SdpOffer,
    pub browser: SocketAddr,
    /// Our address on the browser's network: the WebRTC candidate.
    pub local_ip: IpAddr,
    pub reply: Sender<Result<WebAccepted, WebError>>,
}

pub(crate) struct WebAccepted {
    pub answer: SdpAnswer,
    pub params: SessionParams,
}

/// Why a browser is turned away (the viewer shows it in words).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebError {
    WrongPin,
    PairingClosed,
    TooManyAttempts,
    /// The session could not start (screen, GPU, WebRTC).
    Failed,
}

impl WebError {
    fn code(self) -> &'static str {
        match self {
            WebError::WrongPin => "wrong-pin",
            WebError::PairingClosed => "pairing-closed",
            WebError::TooManyAttempts => "too-many-attempts",
            WebError::Failed => "failed",
        }
    }

    fn status(self) -> u16 {
        match self {
            WebError::Failed => 500,
            _ => 403,
        }
    }
}

/// Forwards a web session's encoded frames to the WebRTC thread, with the
/// host's share of the overlay once per second.
pub(crate) fn send_loop(
    shared: &Shared,
    queue: &Receiver<EncodedFrame>,
    free_enc: &Sender<EncodedFrame>,
    out: &Sender<WebOut>,
    params: SessionParams,
    codec: &'static str,
) {
    let mut window = Instant::now();
    let (mut frames, mut bytes, mut capture_us, mut encode_us) = (0u64, 0u64, 0u64, 0u64);
    // The picture's size changes with the monitor shown.
    let mut size = (params.width, params.height);
    while let Ok(enc) = queue.recv() {
        if !shared.running.load(Ordering::Acquire) {
            break;
        }
        if enc.width > 0 && enc.height > 0 {
            size = (enc.width, enc.height);
        }
        frames += 1;
        bytes += enc.data.len() as u64;
        capture_us += enc.capture_ready_us.saturating_sub(enc.capture_us);
        encode_us += enc.encoded_us.saturating_sub(enc.capture_ready_us);
        let video = WebOut::Video {
            data: enc.data.clone(),
            capture_us: enc.capture_us,
        };
        let _ = free_enc.send(enc);
        if out.send(video).is_err() {
            break;
        }
        HostStats::bump(&shared.stats.frames_sent);
        let elapsed = window.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            let stats = json!({
                "type": "stats",
                "captureUs": capture_us / frames,
                "encodeUs": encode_us / frames,
                "fps": frames as f64 / secs,
                "bitrateBps": (bytes as f64 * 8.0 / secs) as u64,
                "width": size.0,
                "height": size.1,
                "codec": codec,
            });
            let _ = out.try_send(WebOut::Text(stats.to_string()));
            window = Instant::now();
            (frames, bytes, capture_us, encode_us) = (0, 0, 0, 0);
        }
    }
}

/// A new channel for a web session's output.
pub(crate) fn out_channel() -> (Sender<WebOut>, Receiver<WebOut>) {
    bounded(OUT_QUEUE)
}
