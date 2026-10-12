//! Applying the client's input: uinput devices that follow the monitor
//! shown.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use fernsicht_input::uinput::Uinput;
use fernsicht_input::{InputSink, Rumble, RumbleOut};

use crate::config::InputKind;
use crate::shared::Shared;

pub(crate) fn make_input(kind: &InputKind, shared: &Arc<Shared>) -> Option<Box<dyn InputSink>> {
    match kind {
        InputKind::Off => None,
        InputKind::Record(r) => Some(Box::new(r.clone())),
        InputKind::Uinput => match Uinput::open(input_area(shared.screen())) {
            Ok(u) => Some(Box::new(FollowScreen {
                uinput: u,
                shared: shared.clone(),
                seen: shared.screen_changes.load(Ordering::Acquire),
            })),
            Err(e) => {
                log::error!("no input: {e}");
                None
            }
        },
    }
}

fn input_area(screen: Option<fernsicht_capture::ScreenInfo>) -> fernsicht_input::uinput::AbsArea {
    screen
        .map(|s| crate::layout::input_area(&s.connector, &s.active))
        .unwrap_or_default()
}

/// Uinput that maps the pointer onto whichever monitor the session shows
/// now: after a switch the area moves along.
struct FollowScreen {
    uinput: Uinput,
    shared: Arc<Shared>,
    seen: u64,
}

impl InputSink for FollowScreen {
    fn inject(&mut self, event: &fernsicht_input::InputEvent) -> Result<(), String> {
        let now = self.shared.screen_changes.load(Ordering::Acquire);
        if now != self.seen {
            self.seen = now;
            self.uinput.set_area(input_area(self.shared.screen()));
        }
        // Where the pointer goes: the capture thread learns the pointer
        // image's hotspot from it (see `pointer::hotspot`).
        if let fernsicht_input::InputEvent::MouseAbs { x, y } = *event {
            self.shared.note_pointer(x, y);
        }
        self.uinput.inject(event)
    }

    fn release_all(&mut self) {
        self.uinput.release_all();
    }

    fn on_rumble(&mut self, out: RumbleOut) {
        self.uinput.on_rumble(out);
    }
}

/// A rumble of the session's pads as the packet that carries it.
pub(crate) fn rumble_packet(session_id: u32, r: Rumble) -> fernsicht_proto::Rumble {
    fernsicht_proto::Rumble {
        session_id,
        pad: r.pad,
        low: r.low,
        high: r.high,
        duration_ms: u16::try_from(r.ms).unwrap_or(u16::MAX),
    }
}

/// Where the session's pads rumble to: the native client (browsers get
/// none: the web viewer has no use for the packet yet).
pub(crate) fn rumble_out(link: &crate::link::Link, session_id: u32) -> Option<RumbleOut> {
    if matches!(link, crate::link::Link::Web(_)) {
        return None;
    }
    let link = link.clone();
    Some(Arc::new(move |r| {
        let mut buf = [0u8; fernsicht_proto::Rumble::LEN];
        let n = rumble_packet(session_id, r).encode(&mut buf);
        if let Err(e) = link.send(&buf[..n]) {
            log::debug!("rumble: {e}");
        }
    }))
}
