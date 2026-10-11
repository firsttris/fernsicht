//! Input to the host: events from the window, queued and sent sealed,
//! key chords and the pointer's place in the stream.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use fernsicht_input::{InputEvent, InputQueue};
use fernsicht_proto::{InputHeader, MAX_DATAGRAM};
use fernsicht_secure::session::Transport;

use crate::network::send_packet;

/// The window's side of input: events go in here and out to the host at
/// once from the pushing thread; the network thread resends what the host
/// did not acknowledge.
#[derive(Debug, Default)]
pub struct InputHandle {
    pub(crate) queue: Mutex<InputQueue>,
    /// The streamed screen's size, once the host said it.
    stream: Mutex<Option<(u32, u32)>>,
    /// Where events go while a session runs.
    link: Mutex<Option<InputLink>>,
}

/// The session's way to the host, for sending input from any thread
/// (the socket and the transport take `&self`; seal's counter is atomic).
#[derive(Clone)]
pub(crate) struct InputLink {
    pub(crate) socket: Arc<UdpSocket>,
    pub(crate) session_id: u32,
    pub(crate) crypto: Option<Arc<Transport>>,
}

impl std::fmt::Debug for InputLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputLink")
            .field("session_id", &self.session_id)
            .field("sealed", &self.crypto.is_some())
            .finish()
    }
}

impl InputLink {
    /// Sends an encoded input packet (sealed in a secure session).
    fn send(&self, packet: &[u8]) {
        send_packet(
            &self.socket,
            self.crypto.as_deref().map(|t| (t, self.session_id)),
            packet,
        );
    }
}

impl InputHandle {
    /// Queues `event` and, while a session runs, sends it right away: the
    /// network thread wakes only every few milliseconds.
    pub fn push(&self, event: InputEvent) {
        let link = self.link.lock().ok().and_then(|l| l.clone());
        let mut out = [0u8; MAX_DATAGRAM];
        let n = self.queue.lock().ok().and_then(|mut q| {
            q.push(event);
            // Only with a session: `due` marks the events as sent.
            let id = link.as_ref()?.session_id;
            q.due(Instant::now())
                .map(|events| InputHeader::encode(id, events, &mut out))
        });
        // Sent after the queue is let go.
        if let (Some(link), Some(n)) = (link, n) {
            link.send(&out[..n]);
        }
    }

    pub(crate) fn connect(&self, link: Option<InputLink>) {
        if let Ok(mut l) = self.link.lock() {
            *l = link;
        }
    }

    /// Lets go of every key and button held (window lost focus or closes).
    pub fn release_all(&self) {
        if let Ok(mut q) = self.queue.lock() {
            q.release_all();
        }
    }

    /// The pointer at `pos` in a window of `window` pixels, if it is on
    /// the picture.
    pub fn pointer_at(&self, pos: (f64, f64), window: (u32, u32)) -> Option<InputEvent> {
        let stream = (*self.stream.lock().ok()?)?;
        let (x, y) = window_to_stream(pos, window, stream)?;
        Some(InputEvent::MouseAbs { x, y })
    }

    pub(crate) fn set_stream(&self, size: (u32, u32)) {
        if let Ok(mut s) = self.stream.lock() {
            *s = Some(size);
        }
    }
}

/// Gamepad events for the host while `focused` says the window has the
/// focus, dropped otherwise: pads follow the focus like keyboard and
/// mouse (losing it releases everything; a pad must not play on the host
/// while the user does something else here).
pub fn pad_events_while_focused(
    input: Arc<InputHandle>,
    focused: Arc<AtomicBool>,
) -> impl FnMut(InputEvent) + Send + 'static {
    move |e| {
        if focused.load(Ordering::Acquire) {
            input.push(e);
        }
    }
}

/// Where a window position falls on the stream (shown letterboxed in the
/// window), as 0..=65535 each way; `None` on the black bars.
pub fn window_to_stream(
    pos: (f64, f64),
    window: (u32, u32),
    stream: (u32, u32),
) -> Option<(u16, u16)> {
    let [sx, sy] = fernsicht_render::letterbox_size(stream, window);
    let (ww, wh) = (f64::from(window.0.max(1)), f64::from(window.1.max(1)));
    // The picture's size and top-left corner in window pixels.
    let (pw, ph) = (ww * f64::from(sx), wh * f64::from(sy));
    let (left, top) = ((ww - pw) / 2.0, (wh - ph) / 2.0);
    let (u, v) = ((pos.0 - left) / pw, (pos.1 - top) / ph);
    if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
        return None;
    }
    let q = |t: f64| (t * f64::from(u16::MAX)).round() as u16;
    Some((q(u), q(v)))
}

/// Most keys in one combination sent from a menu.
pub const MAX_CHORD: usize = 6;

/// A key combination as events: pressed in order, released in reverse
/// (Ctrl, Alt, Del → Del, Alt, Ctrl up).
pub fn chord_events(codes: &[u16]) -> Vec<InputEvent> {
    let press = codes.iter().map(|&code| InputEvent::Key {
        code,
        pressed: true,
    });
    let release = codes.iter().rev().map(|&code| InputEvent::Key {
        code,
        pressed: false,
    });
    press.chain(release).collect()
}

/// The app's "keys 29,56,111" command (Linux key codes, at most
/// [`MAX_CHORD`], no repeats).
pub fn parse_keys_command(line: &str) -> Option<Vec<u16>> {
    let list = line.strip_prefix("keys ")?;
    let codes: Vec<u16> = list
        .split(',')
        .map(|c| c.trim().parse().ok())
        .collect::<Option<_>>()?;
    let valid = !codes.is_empty()
        && codes.len() <= MAX_CHORD
        && codes.iter().all(|&code| {
            InputEvent::Key {
                code,
                pressed: true,
            }
            .validate()
            .is_ok()
        })
        && codes
            .iter()
            .enumerate()
            .all(|(i, c)| !codes[..i].contains(c));
    valid.then_some(codes)
}
