//! The desktop's clipboard, shared with a client that may control the
//! desktop (input on; a view-only session reads nothing): text copied
//! here goes to the client, text copied there is set here. The host runs
//! as root, the clipboard is the desktop user's: their compositor's
//! socket (`/run/user/<uid>/wayland-0`) and data control
//! ([`fernsicht_clipboard`]). When a session starts, the client's
//! clipboard wins: what the desktop holds then is not sent.

use std::path::PathBuf;

use fernsicht_clipboard::{Clipboard, Current, Place, Sync};
use fernsicht_proto::{ClipboardAck, ClipboardChunk, MAX_DATAGRAM};

use crate::link::Link;

/// Pieces not acknowledged go again after this long.
const RESEND_US: u64 = 300_000;

pub(crate) struct SessionClipboard {
    clipboard: Clipboard,
    sync: Sync,
}

/// The desktop user's compositor socket.
fn desktop_socket() -> Option<PathBuf> {
    let dir = fernsicht_core::desktop::desktop_user()?.runtime_dir();
    let names: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let name = fernsicht_clipboard::wayland_socket(names.iter().map(String::as_str))?;
    Some(dir.join(name))
}

/// The clipboard for a session of `place`, where there is one.
pub(crate) fn open(place: &Place) -> Option<SessionClipboard> {
    let socket = match place {
        Place::Off => return None,
        Place::Here => desktop_socket()?,
        Place::At(p) => p.clone(),
    };
    match Clipboard::at(&socket, Current::Skip) {
        Ok(clipboard) => {
            log::info!("clipboard shared ({})", socket.display());
            Some(SessionClipboard {
                clipboard,
                sync: Sync::default(),
            })
        }
        Err(e) => {
            log::info!("no clipboard: {e}");
            None
        }
    }
}

impl SessionClipboard {
    /// What was copied here goes to the client; pieces not acknowledged
    /// go again.
    pub(crate) fn tick(&mut self, link: &Link, session_id: u32, now_us: u64) {
        while let Ok(text) = self.clipboard.changes().try_recv() {
            self.sync.copied(text);
        }
        let mut buf = [0u8; MAX_DATAGRAM];
        for (serial, total, offset, data) in self.sync.due(now_us, RESEND_US) {
            let n = ClipboardChunk {
                session_id,
                serial,
                total,
                offset,
            }
            .encode(data, &mut buf);
            let _ = link.send(&buf[..n]);
        }
    }

    /// A piece of the client's text: acknowledged, and once whole, set
    /// here.
    pub(crate) fn piece(&mut self, link: &Link, c: &ClipboardChunk, data: &[u8]) {
        let reply = self.sync.piece(c.serial, c.total, c.offset, data);
        if let Some((serial, upto)) = reply.ack {
            let mut buf = [0u8; ClipboardAck::LEN];
            let n = ClipboardAck {
                session_id: c.session_id,
                serial,
                upto,
            }
            .encode(&mut buf);
            let _ = link.send(&buf[..n]);
        }
        if let Some(text) = reply.text {
            self.clipboard.set(text);
        }
    }

    pub(crate) fn acked(&mut self, a: &ClipboardAck) {
        self.sync.acked(a.serial, a.upto);
    }
}
