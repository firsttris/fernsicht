//! Applying the client's input: uinput devices that follow the monitor
//! shown.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use fernsicht_input::InputSink;
use fernsicht_input::uinput::Uinput;

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
        self.uinput.inject(event)
    }

    fn release_all(&mut self) {
        self.uinput.release_all();
    }
}
