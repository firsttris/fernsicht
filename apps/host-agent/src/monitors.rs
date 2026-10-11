//! Telling the client which monitors the host has.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use fernsicht_proto::clip;

use crate::link::Link;
use crate::shared::Shared;

/// The monitor list is sent again this often, so that a lost one heals.
pub(crate) const MONITORS_REPEAT: Duration = Duration::from_secs(2);

/// Tells the client which monitors the host has and which it sees: at the
/// start, after every switch, and every [`MONITORS_REPEAT`]. A browser gets
/// it as JSON on the data channel.
pub(crate) struct MonitorSender {
    link: Link,
    /// Screen change count and time of the last list sent.
    sent: Option<(u64, Instant)>,
}

impl MonitorSender {
    pub(crate) fn new(link: Link) -> Self {
        Self { link, sent: None }
    }

    pub(crate) fn tick(&mut self, shared: &Shared) {
        let changes = shared.screen_changes.load(Ordering::Acquire);
        if self
            .sent
            .is_some_and(|(c, at)| c == changes && at.elapsed() < MONITORS_REPEAT)
        {
            return;
        }
        self.sent = Some((changes, Instant::now()));
        let Some(screen) = shared.screen() else {
            return;
        };
        let Some((current, list)) = monitor_list(&screen) else {
            return;
        };
        self.link.send_monitors(current, &list);
    }
}

/// The monitors as the protocol carries them (names clipped, at most
/// [`fernsicht_proto::MAX_MONITORS`]) and the index of the one shown.
pub(crate) fn monitor_list(
    screen: &fernsicht_capture::ScreenInfo,
) -> Option<(u8, Vec<fernsicht_proto::Monitor<'_>>)> {
    let clamp = |v: u32| v.min(u32::from(u16::MAX)) as u16;
    let list: Vec<_> = screen
        .active
        .iter()
        .zip(screen.sizes.iter().chain(std::iter::repeat(&(0, 0))))
        .take(fernsicht_proto::MAX_MONITORS)
        .map(|(name, &(w, h))| fernsicht_proto::Monitor {
            name: clip(name, fernsicht_proto::MAX_MONITOR_NAME),
            width: clamp(w),
            height: clamp(h),
        })
        .filter(|m| !m.name.is_empty())
        .collect();
    let current = screen.active.iter().position(|n| *n == screen.connector)?;
    (current < list.len()).then_some((current as u8, list))
}
