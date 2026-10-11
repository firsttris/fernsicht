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
use fernsicht_proto::Monitor;
use serde_json::json;
use str0m::change::{SdpAnswer, SdpOffer};

use crate::shared::Shared;
use crate::throughput::{self, Sent, Window};
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

/// How often the browser's overlay gets the host's numbers.
const STATS_EVERY: Duration = Duration::from_secs(1);

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
    let mut window: Window = (Instant::now(), Sent::default());
    // The picture's size changes with the monitor shown.
    let mut size = (params.width, params.height);
    while let Ok(enc) = queue.recv() {
        if !shared.running.load(Ordering::Acquire) {
            break;
        }
        if enc.width > 0 && enc.height > 0 {
            size = (enc.width, enc.height);
        }
        window.1 = throughput::count(window.1, &enc, enc.data.len() as u64, 0);
        let video = WebOut::Video {
            data: enc.data.clone(),
            capture_us: enc.capture_us,
        };
        let _ = free_enc.send(enc);
        if out.send(video).is_err() {
            break;
        }
        HostStats::bump(&shared.stats.frames_sent);
        if let Some(r) = throughput::close_after(&mut window, Instant::now(), STATS_EVERY) {
            let stats = json!({
                "type": "stats",
                "captureUs": r.capture_us,
                "encodeUs": r.encode_us,
                "fps": r.fps,
                "bitrateBps": r.bits_per_sec as u64,
                "width": size.0,
                "height": size.1,
                "codec": codec,
            });
            let _ = out.try_send(WebOut::Text(stats.to_string()));
        }
    }
}

/// A new channel for a web session's output.
pub(crate) fn out_channel() -> (Sender<WebOut>, Receiver<WebOut>) {
    bounded(OUT_QUEUE)
}

/// The host's monitors for the data channel, and which one is shown.
pub(crate) fn monitors_message(current: u8, list: &[Monitor<'_>]) -> String {
    let list: Vec<_> = list
        .iter()
        .map(|m| json!({"name": m.name, "width": m.width, "height": m.height}))
        .collect();
    json!({"type": "monitors", "current": current, "list": list}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitors_go_to_the_browser_as_json() {
        let list = [
            Monitor {
                name: "DP-1",
                width: 2560,
                height: 1440,
            },
            Monitor {
                name: "HDMI-A-1",
                width: 1920,
                height: 1080,
            },
        ];
        let v: serde_json::Value = serde_json::from_str(&monitors_message(1, &list)).unwrap();
        assert_eq!(
            v,
            json!({"type": "monitors", "current": 1, "list": [
                {"name": "DP-1", "width": 2560, "height": 1440},
                {"name": "HDMI-A-1", "width": 1920, "height": 1080},
            ]})
        );
    }
}
