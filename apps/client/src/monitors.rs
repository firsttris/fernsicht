//! The host's monitors and asking for another one.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The host's monitors, as its last [`fernsicht_proto::Monitors`] said.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorList {
    /// Index of the one shown.
    pub current: u8,
    pub list: Vec<Monitor>,
}

/// One of the host's monitors.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monitor {
    /// The connector, e.g. `DP-1`.
    pub name: String,
    pub width: u16,
    pub height: u16,
}

impl MonitorList {
    /// The index `request` means in this list, if any.
    pub fn resolve(&self, request: MonitorRequest) -> Option<u8> {
        let n = self.list.len() as u8;
        if n == 0 {
            return None;
        }
        Some(match request {
            MonitorRequest::Index(i) if i < n => i,
            MonitorRequest::Index(_) => return None,
            MonitorRequest::Next => (self.current + 1) % n,
            MonitorRequest::Previous => (self.current + n - 1) % n,
        })
    }
}

/// Which monitor to show next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorRequest {
    Index(u8),
    Next,
    Previous,
}

/// Shared between the network thread (fills the list, sends requests) and
/// the window or the app (reads the list, asks for switches).
#[derive(Debug, Default)]
pub struct MonitorControl {
    list: Mutex<Option<MonitorList>>,
    request: Mutex<Option<MonitorRequest>>,
}

impl MonitorControl {
    pub fn list(&self) -> Option<MonitorList> {
        self.list.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn request(&self, r: MonitorRequest) {
        *self.request.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
    }

    pub(crate) fn set_list(&self, list: MonitorList) {
        *self.list.lock().unwrap_or_else(|e| e.into_inner()) = Some(list);
    }

    pub(crate) fn take_request(&self) -> Option<MonitorRequest> {
        self.request
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

/// The app's "monitor 1" command.
pub fn parse_monitor_command(line: &str) -> Option<MonitorRequest> {
    match line.strip_prefix("monitor ")?.trim() {
        "next" => Some(MonitorRequest::Next),
        "previous" => Some(MonitorRequest::Previous),
        n => n
            .parse::<u8>()
            .ok()
            .filter(|&i| usize::from(i) < fernsicht_proto::MAX_MONITORS)
            .map(MonitorRequest::Index),
    }
}
