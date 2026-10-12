//! The clipboard of a Wayland desktop through data control: the protocol
//! for clipboard managers, which reads and sets the selection without a
//! window or focus (`ext-data-control-v1`, else wlroots'
//! `zwlr-data-control-unstable-v1`). KWin, Sway, Hyprland and COSMIC
//! offer it; GNOME does not, and there is no clipboard then.
//!
//! One thread owns the connection. It sees every new selection, reads its
//! text through a pipe (on a thread of its own: the other client writes
//! at its pace) and hands it out; it sets the selection by offering a
//! source, which it serves while it lasts. Its own selection is not read
//! back.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as ext_dev, ext_data_control_manager_v1 as ext_mgr,
    ext_data_control_offer_v1 as ext_offer, ext_data_control_source_v1 as ext_src,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as wlr_dev, zwlr_data_control_manager_v1 as wlr_mgr,
    zwlr_data_control_offer_v1 as wlr_offer, zwlr_data_control_source_v1 as wlr_src,
};

use crate::transfer::MAX_TEXT;

/// Text types, best first: what desktops offer for text (X11 apps through
/// Xwayland say `UTF8_STRING`).
pub const TEXT_TYPES: [&str; 4] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
];

/// The best text type among those a selection offers.
pub fn pick_text_type(offered: &[String]) -> Option<&'static str> {
    TEXT_TYPES
        .into_iter()
        .find(|t| offered.iter().any(|o| o == t))
}

/// How long the thread sleeps at most between looks at its commands.
const WAIT_MS: i32 = 50;

enum Command {
    Set(String),
}

/// Whether the text the clipboard holds when it is opened counts as a
/// change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Current {
    /// It comes as the first of [`Clipboard::changes`].
    Report,
    /// Only what is copied from now on (the host: the client's clipboard
    /// wins when a session starts).
    Skip,
}

/// A desktop's clipboard, until dropped.
pub struct Clipboard {
    commands: Sender<Command>,
    changes: Receiver<String>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Clipboard {
    /// The clipboard of the compositor listening on `socket` (the host:
    /// the desktop user's `wayland-0`).
    pub fn at(socket: &Path, current: Current) -> Result<Self, String> {
        let stream = UnixStream::connect(socket)
            .map_err(|e| format!("connect to {}: {e}", socket.display()))?;
        let conn =
            Connection::from_socket(stream).map_err(|e| format!("Wayland connection: {e}"))?;
        Self::start(conn, current)
    }

    /// The clipboard of this session's compositor (`WAYLAND_DISPLAY`).
    pub fn of_session(current: Current) -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("no Wayland session: {e}"))?;
        Self::start(conn, current)
    }

    fn start(conn: Connection, current: Current) -> Result<Self, String> {
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| format!("Wayland registry: {e}"))?;
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=7, ())
            .map_err(|e| format!("no seat: {e}"))?;
        let control =
            if let Ok(m) = globals.bind::<ext_mgr::ExtDataControlManagerV1, _, _>(&qh, 1..=1, ()) {
                let device = m.get_data_device(&seat, &qh, ());
                Control::Ext(m, device)
            } else if let Ok(m) =
                globals.bind::<wlr_mgr::ZwlrDataControlManagerV1, _, _>(&qh, 1..=2, ())
            {
                let device = m.get_data_device(&seat, &qh, ());
                Control::Wlr(m, device)
            } else {
                return Err("the compositor offers no data control (GNOME does not)".into());
            };
        let (commands, command_rx) = channel();
        let (change_tx, changes) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let mut state = State {
            control,
            offers: HashMap::new(),
            pending: Vec::new(),
            source: None,
            serving: Arc::from(""),
            finished: false,
        };
        // The device's first events: the selection there is now.
        queue
            .roundtrip(&mut state)
            .map_err(|e| format!("Wayland roundtrip: {e}"))?;
        if current == Current::Skip {
            state
                .pending
                .drain(..)
                .for_each(|(offer, _)| offer.destroy());
        }
        let thread = std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                run(&conn, &mut queue, &mut state, &command_rx, &change_tx, &s);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            commands,
            changes,
            stop,
            thread: Some(thread),
        })
    }

    /// Makes `text` the desktop's clipboard.
    pub fn set(&self, text: String) {
        let _ = self.commands.send(Command::Set(text));
    }

    /// Texts copied on the desktop (not those [`Self::set`] put there),
    /// the current one first.
    pub fn changes(&self) -> &Receiver<String> {
        &self.changes
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    commands: &Receiver<Command>,
    changes: &Sender<String>,
    stop: &AtomicBool,
) {
    let qh = queue.handle();
    while !stop.load(Ordering::Relaxed) && !state.finished {
        while let Ok(Command::Set(text)) = commands.try_recv() {
            state.set(text, &qh);
        }
        if queue.dispatch_pending(state).is_err() || queue.flush().is_err() {
            return;
        }
        if let Some(guard) = queue.prepare_read() {
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut fd, 1, WAIT_MS) } > 0;
            if ready && guard.read().is_err() {
                log::info!("clipboard: the compositor closed the connection");
                return;
            }
        }
        if queue.dispatch_pending(state).is_err() {
            return;
        }
        for (offer, mime) in std::mem::take(&mut state.pending) {
            read_offer(conn, offer, mime, changes.clone());
        }
    }
}

/// Asks the selection's owner for its text and reads it on a thread of
/// its own; hands it to `changes` if it is text of a size that goes
/// across.
fn read_offer(conn: &Connection, offer: Offer, mime: &'static str, changes: Sender<String>) {
    let Ok((mut reader, writer)) = std::io::pipe() else {
        offer.destroy();
        return;
    };
    offer.receive(mime, OwnedFd::from(writer).as_fd());
    offer.destroy();
    // The request must leave before our copy of the write end is closed
    // (dropped above, after receive() took it into the message).
    let _ = conn.flush();
    let _ = std::thread::Builder::new()
        .name("clipboard read".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let limit = (MAX_TEXT + 1) as u64;
            if (&mut reader).take(limit).read_to_end(&mut bytes).is_err() || bytes.len() > MAX_TEXT
            {
                return;
            }
            if let Ok(text) = String::from_utf8(bytes) {
                let _ = changes.send(text);
            }
        });
}

/// The protocol the compositor has: its manager and our device.
enum Control {
    Ext(
        ext_mgr::ExtDataControlManagerV1,
        ext_dev::ExtDataControlDeviceV1,
    ),
    Wlr(
        wlr_mgr::ZwlrDataControlManagerV1,
        wlr_dev::ZwlrDataControlDeviceV1,
    ),
}

enum Offer {
    Ext(ext_offer::ExtDataControlOfferV1),
    Wlr(wlr_offer::ZwlrDataControlOfferV1),
}

impl Offer {
    fn id(&self) -> ObjectId {
        match self {
            Offer::Ext(o) => o.id(),
            Offer::Wlr(o) => o.id(),
        }
    }

    fn receive(&self, mime: &str, fd: std::os::fd::BorrowedFd<'_>) {
        match self {
            Offer::Ext(o) => o.receive(mime.into(), fd),
            Offer::Wlr(o) => o.receive(mime.into(), fd),
        }
    }

    fn destroy(&self) {
        match self {
            Offer::Ext(o) => o.destroy(),
            Offer::Wlr(o) => o.destroy(),
        }
    }
}

enum Source {
    Ext(ext_src::ExtDataControlSourceV1),
    Wlr(wlr_src::ZwlrDataControlSourceV1),
}

impl Source {
    fn id(&self) -> ObjectId {
        match self {
            Source::Ext(s) => s.id(),
            Source::Wlr(s) => s.id(),
        }
    }

    fn destroy(&self) {
        match self {
            Source::Ext(s) => s.destroy(),
            Source::Wlr(s) => s.destroy(),
        }
    }
}

struct State {
    control: Control,
    /// Offers announced, with the types each offers.
    offers: HashMap<ObjectId, Vec<String>>,
    /// Selections to read (after the events are handled).
    pending: Vec<(Offer, &'static str)>,
    /// Our source while it is the selection.
    source: Option<Source>,
    /// The text it serves.
    serving: Arc<str>,
    finished: bool,
}

impl State {
    /// Offers `text` as the selection.
    fn set(&mut self, text: String, qh: &QueueHandle<Self>) {
        if let Some(old) = self.source.take() {
            old.destroy();
        }
        self.serving = Arc::from(text);
        let source = match &self.control {
            Control::Ext(m, d) => {
                let s = m.create_data_source(qh, ());
                TEXT_TYPES.iter().for_each(|t| s.offer((*t).into()));
                d.set_selection(Some(&s));
                Source::Ext(s)
            }
            Control::Wlr(m, d) => {
                let s = m.create_data_source(qh, ());
                TEXT_TYPES.iter().for_each(|t| s.offer((*t).into()));
                d.set_selection(Some(&s));
                Source::Wlr(s)
            }
        };
        self.source = Some(source);
    }

    /// A new selection: ours (not read back), text (read), or else.
    fn selected(&mut self, offer: Option<Offer>) {
        let Some(offer) = offer else { return };
        let offered = self.offers.remove(&offer.id()).unwrap_or_default();
        match pick_text_type(&offered) {
            Some(mime) if self.source.is_none() => self.pending.push((offer, mime)),
            _ => offer.destroy(),
        }
    }

    /// Someone reads our selection: written on a thread of its own.
    fn serve(&self, fd: OwnedFd) {
        let text = self.serving.clone();
        let _ = std::thread::Builder::new()
            .name("clipboard write".into())
            .spawn(move || {
                let _ = std::fs::File::from(fd).write_all(text.as_bytes());
            });
    }

    /// Our source is no longer the selection (someone copied).
    fn cancelled(&mut self, id: &ObjectId) {
        if self.source.as_ref().is_some_and(|s| &s.id() == id) {
            self.source = None;
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// The same handlers for both protocols: their events are alike.
macro_rules! data_control {
    ($mgr:ident :: $Mgr:ident, $dev:ident :: $Dev:ident, $offer:ident :: $Offer:ident,
     $src:ident :: $Src:ident, $variant:ident) => {
        impl Dispatch<$mgr::$Mgr, ()> for State {
            fn event(
                _: &mut Self,
                _: &$mgr::$Mgr,
                _: $mgr::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }

        impl Dispatch<$dev::$Dev, ()> for State {
            fn event(
                state: &mut Self,
                _: &$dev::$Dev,
                event: $dev::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    $dev::Event::DataOffer { id } => {
                        state.offers.insert(id.id(), Vec::new());
                    }
                    $dev::Event::Selection { id } => state.selected(id.map(Offer::$variant)),
                    $dev::Event::PrimarySelection { id: Some(o) } => {
                        state.offers.remove(&o.id());
                        o.destroy();
                    }
                    $dev::Event::Finished => state.finished = true,
                    _ => {}
                }
            }

            event_created_child!(State, $dev::$Dev, [
                $dev::EVT_DATA_OFFER_OPCODE => ($offer::$Offer, ()),
            ]);
        }

        impl Dispatch<$offer::$Offer, ()> for State {
            fn event(
                state: &mut Self,
                offer: &$offer::$Offer,
                event: $offer::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let $offer::Event::Offer { mime_type } = event {
                    state.offers.entry(offer.id()).or_default().push(mime_type);
                }
            }
        }

        impl Dispatch<$src::$Src, ()> for State {
            fn event(
                state: &mut Self,
                source: &$src::$Src,
                event: $src::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    $src::Event::Send { fd, .. } => state.serve(fd),
                    $src::Event::Cancelled => {
                        state.cancelled(&source.id());
                        source.destroy();
                    }
                    _ => {}
                }
            }
        }
    };
}

data_control!(
    ext_mgr::ExtDataControlManagerV1,
    ext_dev::ExtDataControlDeviceV1,
    ext_offer::ExtDataControlOfferV1,
    ext_src::ExtDataControlSourceV1,
    Ext
);
data_control!(
    wlr_mgr::ZwlrDataControlManagerV1,
    wlr_dev::ZwlrDataControlDeviceV1,
    wlr_offer::ZwlrDataControlOfferV1,
    wlr_src::ZwlrDataControlSourceV1,
    Wlr
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_comes_in_the_best_type_offered() {
        let offered = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            pick_text_type(&offered(&["image/png", "text/plain", "UTF8_STRING"])),
            Some("UTF8_STRING")
        );
        assert_eq!(
            pick_text_type(&offered(&["text/plain;charset=utf-8", "TEXT"])),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(pick_text_type(&offered(&["image/png"])), None, "not text");
    }
}
