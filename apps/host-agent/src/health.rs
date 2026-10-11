//! Which errors a session rides out and when it ends: pure decisions for
//! the pipeline threads and the control loop, and the guard that turns a
//! thread that stopped into the end of its session.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A send error that is only packet loss: the network is gone for a
/// moment (Wi-Fi roaming, a VPN reconnecting), a buffer is full, or a
/// firewall dropped the packet. FEC and keyframes repair the picture once
/// packets get through again; anything else ends the send thread.
pub fn send_error_is_transient(e: &io::Error) -> bool {
    use io::ErrorKind::*;
    matches!(
        e.kind(),
        WouldBlock
            | Interrupted
            | TimedOut
            | NetworkUnreachable
            | HostUnreachable
            | NetworkDown
            | ConnectionRefused
            | AddrNotAvailable
    ) || matches!(
        e.raw_os_error(),
        Some(libc::ENOBUFS | libc::EPERM | libc::EHOSTDOWN)
    )
}

/// How long the capture thread waits after `failures` errors in a row
/// before it tries again: a few milliseconds for a framebuffer that
/// vanished during a mode change, at most 100 ms for a monitor in standby
/// (a key press from the client wakes it, and the picture should be back
/// at once; a try costs a few ioctls).
pub fn capture_retry_delay(failures: u32) -> Duration {
    Duration::from_millis((5u64 << failures.min(5)).min(100))
}

/// Why the control loop ends a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// No packet from the client for longer than the timeout.
    TimedOut,
    /// The browser left (it ends its own session).
    BrowserLeft,
    /// A pipeline thread stopped; the client is told with a Bye instead
    /// of watching a frozen picture.
    StreamStopped,
}

/// Whether a session has ended, from what the control loop knows of it.
pub fn session_end(
    web: bool,
    running: bool,
    silent_for: Duration,
    timeout: Duration,
) -> Option<End> {
    match (web, running) {
        (true, false) => Some(End::BrowserLeft),
        (false, false) => Some(End::StreamStopped),
        (false, true) if silent_for > timeout => Some(End::TimedOut),
        _ => None,
    }
}

impl End {
    pub fn describe(self) -> &'static str {
        match self {
            End::TimedOut => "client timed out",
            End::BrowserLeft => "browser left",
            End::StreamStopped => "stream stopped",
        }
    }
}

/// Held by a pipeline thread: when the thread ends while its session
/// should still run (an error it could not ride out, or a panic), it
/// clears `running`, and the control loop ends the session.
pub struct StopOnExit<'a> {
    pub running: &'a AtomicBool,
    pub thread: &'static str,
}

impl Drop for StopOnExit<'_> {
    fn drop(&mut self) {
        if self.running.swap(false, Ordering::AcqRel) {
            log::warn!("{}: stopped; ending the session", self.thread);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_blips_are_only_loss() {
        for e in [
            io::Error::from(io::ErrorKind::WouldBlock),
            io::Error::from(io::ErrorKind::NetworkUnreachable),
            io::Error::from(io::ErrorKind::HostUnreachable),
            io::Error::from(io::ErrorKind::NetworkDown),
            io::Error::from_raw_os_error(libc::ENETUNREACH),
            io::Error::from_raw_os_error(libc::EHOSTUNREACH),
            io::Error::from_raw_os_error(libc::ENOBUFS),
            io::Error::from_raw_os_error(libc::EPERM),
            io::Error::from_raw_os_error(libc::EHOSTDOWN),
        ] {
            assert!(send_error_is_transient(&e), "{e}");
        }
    }

    #[test]
    fn broken_sockets_and_seal_errors_end_the_thread() {
        for e in [
            io::Error::from_raw_os_error(libc::EBADF),
            io::Error::from_raw_os_error(libc::EMSGSIZE),
            io::Error::from_raw_os_error(libc::EACCES),
            io::Error::other("seal: counter exhausted"),
        ] {
            assert!(!send_error_is_transient(&e), "{e}");
        }
    }

    #[test]
    fn capture_retries_back_off_to_a_tenth_of_a_second() {
        let delays: Vec<u64> = (0..8)
            .map(|n| capture_retry_delay(n).as_millis() as u64)
            .collect();
        assert_eq!(delays, [5, 10, 20, 40, 80, 100, 100, 100]);
        assert_eq!(capture_retry_delay(u32::MAX), Duration::from_millis(100));
    }

    #[test]
    fn sessions_end_for_the_right_reason() {
        let t = Duration::from_secs(5);
        let (quiet, silent) = (Duration::from_secs(1), Duration::from_secs(6));
        assert_eq!(session_end(false, true, quiet, t), None);
        assert_eq!(session_end(false, true, silent, t), Some(End::TimedOut));
        assert_eq!(
            session_end(false, false, quiet, t),
            Some(End::StreamStopped)
        );
        assert_eq!(session_end(true, false, quiet, t), Some(End::BrowserLeft));
        // A browser's packets never reach our socket: silence is normal.
        assert_eq!(session_end(true, true, silent, t), None);
    }

    #[test]
    fn a_thread_that_stops_ends_its_session_once() {
        let running = AtomicBool::new(true);
        drop(StopOnExit {
            running: &running,
            thread: "send",
        });
        assert!(!running.load(Ordering::Acquire));
        // Already stopped (the session ended normally): nothing more.
        drop(StopOnExit {
            running: &running,
            thread: "encode",
        });
        assert!(!running.load(Ordering::Acquire));
    }

    #[test]
    fn a_panicking_thread_ends_its_session() {
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let r = running.clone();
        let t = std::thread::spawn(move || {
            let _guard = StopOnExit {
                running: &r,
                thread: "capture",
            };
            panic!("boom");
        });
        assert!(t.join().is_err());
        assert!(!running.load(Ordering::Acquire));
    }
}
