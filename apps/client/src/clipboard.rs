//! This machine's clipboard, shared with the host while the session may
//! control it: text copied here goes there, text copied there is set here
//! ([`fernsicht_clipboard`]). What this desktop holds when the session
//! starts goes too: the clipboard of the machine the user sits at wins.

use std::net::UdpSocket;

use fernsicht_clipboard::{Clipboard, Current, Place, Sync};
use fernsicht_proto::{ClipboardAck, ClipboardChunk, MAX_DATAGRAM};
use fernsicht_secure::session::Transport;

use crate::network::send_packet;

/// Pieces not acknowledged go again after this long.
const RESEND_US: u64 = 300_000;

pub(crate) struct SessionClipboard {
    clipboard: Clipboard,
    sync: Sync,
}

/// The clipboard of `place`, where there is one.
pub(crate) fn open(place: &Place) -> Option<SessionClipboard> {
    let opened = match place {
        Place::Off => return None,
        Place::Here => Clipboard::of_session(Current::Report),
        Place::At(socket) => Clipboard::at(socket, Current::Report),
    };
    match opened {
        Ok(clipboard) => {
            log::info!("clipboard shared with the host");
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
    /// What was copied here goes to the host; pieces not acknowledged go
    /// again.
    pub(crate) fn tick(
        &mut self,
        socket: &UdpSocket,
        crypto: Option<(&Transport, u32)>,
        session_id: u32,
        now_us: u64,
    ) {
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
            send_packet(socket, crypto, &buf[..n]);
        }
    }

    /// A piece of the host's text: acknowledged, and once whole, set here.
    pub(crate) fn piece(
        &mut self,
        socket: &UdpSocket,
        crypto: Option<(&Transport, u32)>,
        c: &ClipboardChunk,
        data: &[u8],
    ) {
        let reply = self.sync.piece(c.serial, c.total, c.offset, data);
        if let Some((serial, upto)) = reply.ack {
            let mut buf = [0u8; ClipboardAck::LEN];
            let n = ClipboardAck {
                session_id: c.session_id,
                serial,
                upto,
            }
            .encode(&mut buf);
            send_packet(socket, crypto, &buf[..n]);
        }
        if let Some(text) = reply.text {
            self.clipboard.set(text);
        }
    }

    pub(crate) fn acked(&mut self, a: &ClipboardAck) {
        self.sync.acked(a.serial, a.upto);
    }
}
