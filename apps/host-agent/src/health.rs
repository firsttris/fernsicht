//! Which errors a session rides out and when it ends: pure decisions for
//! the pipeline threads and the control loop, and the guard that turns a
//! thread that stopped into the end of its session.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

/// How long one call into the encoder (encode, bitrate, a new encoder) may
/// take before the encoder counts as hung. A frame takes milliseconds and a
/// new encoder well under a second; after a GPU fault (NVIDIA Xid 13/32)
/// CUDA and NVENC calls can wait forever instead of failing.
pub const ENCODER_HANG: Duration = Duration::from_secs(5);

/// Held around a call into the encoder: `since` holds when it began (µs,
/// [`fernsicht_core::now_us`]), 0 while the encoder is idle.
pub struct Busy<'a>(&'a AtomicU64);

impl<'a> Busy<'a> {
    pub fn enter(since: &'a AtomicU64) -> Self {
        since.store(fernsicht_core::now_us().max(1), Ordering::Release);
        Self(since)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

/// How long the encoder has been in its current call (zero when idle).
pub fn busy_for(since_us: u64, now_us: u64) -> Duration {
    match since_us {
        0 => Duration::ZERO,
        t => Duration::from_micros(now_us.saturating_sub(t)),
    }
}

/// Joins `threads` that end within `wait`; the ones still running then are
/// left behind (stuck in a driver call, they would hold the host forever).
/// How many were left.
pub fn join_within(threads: Vec<JoinHandle<()>>, wait: Duration) -> usize {
    let deadline = Instant::now() + wait;
    while threads.iter().any(|t| !t.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let (done, stuck): (Vec<_>, Vec<_>) = threads.into_iter().partition(|t| t.is_finished());
    for t in done {
        let _ = t.join();
    }
    for t in &stuck {
        log::error!(
            "thread {} does not stop; left behind",
            t.thread().name().unwrap_or("?")
        );
    }
    stuck.len()
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
    /// The client's device was unpaired.
    Unpaired,
    /// Ended on the host (control socket).
    EndedHere,
    /// A call into the encoder has not returned for [`ENCODER_HANG`]: the
    /// GPU is wedged, and the host restarts with a fresh one.
    EncoderHung,
}

/// Whether a session has ended, from what the control loop knows of it.
pub fn session_end(
    web: bool,
    running: bool,
    (silent_for, timeout): (Duration, Duration),
    encoding_for: Duration,
) -> Option<End> {
    if encoding_for > ENCODER_HANG {
        return Some(End::EncoderHung);
    }
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
            End::Unpaired => "its device was unpaired",
            End::EndedHere => "ended on the host",
            End::EncoderHung => "the encoder stopped responding",
        }
    }

    /// Whether the client gets a Bye (it is still there to hear it).
    pub fn tells_the_client(self) -> bool {
        matches!(
            self,
            End::StreamStopped | End::Unpaired | End::EndedHere | End::EncoderHung
        )
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

/// The threads of a session being started. If the start fails half-way,
/// dropping this stops the ones already running (with `stop`) and joins
/// them, instead of leaving them to work for nobody.
pub struct Starting<F: FnMut()> {
    threads: Vec<JoinHandle<()>>,
    stop: Option<F>,
}

impl<F: FnMut()> Starting<F> {
    pub fn new(stop: F) -> Self {
        Self {
            threads: Vec::new(),
            stop: Some(stop),
        }
    }

    pub fn add(&mut self, thread: JoinHandle<()>) {
        self.threads.push(thread);
    }

    /// The start went through: the threads belong to the session now.
    pub fn done(mut self) -> Vec<JoinHandle<()>> {
        self.stop = None;
        std::mem::take(&mut self.threads)
    }
}

impl<F: FnMut()> Drop for Starting<F> {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.as_mut() {
            stop();
            for t in self.threads.drain(..) {
                let _ = t.join();
            }
        }
    }
}

/// Collapses an error that repeats every frame into one log line per second.
#[derive(Default)]
pub(crate) struct RepeatedError {
    last: String,
    repeats: u64,
    since: Option<Instant>,
}

impl RepeatedError {
    /// The line to log for this occurrence, if any.
    pub(crate) fn report(&mut self, msg: String, now: Instant) -> Option<String> {
        if msg != self.last || self.since.is_none() {
            self.last = msg;
            self.repeats = 0;
            self.since = Some(now);
            return Some(self.last.clone());
        }
        self.repeats += 1;
        let since = self.since.expect("set above");
        if now.duration_since(since) < Duration::from_secs(1) {
            return None;
        }
        let line = format!(
            "{} (another {}× in the last second)",
            self.last, self.repeats
        );
        self.repeats = 0;
        self.since = Some(now);
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(running: &std::sync::Arc<AtomicBool>) -> JoinHandle<()> {
        let running = running.clone();
        std::thread::spawn(move || {
            while running.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    }

    #[test]
    fn a_failed_start_stops_and_joins_what_it_started() {
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let start = || -> Result<Vec<JoinHandle<()>>, &'static str> {
            let r = running.clone();
            let mut threads = Starting::new(move || r.store(false, Ordering::Release));
            threads.add(worker(&running));
            threads.add(worker(&running));
            Err("the third thread could not start")?;
            Ok(threads.done())
        };
        assert!(start().is_err());
        // Joined: both workers saw the stop and ended.
        assert!(!running.load(Ordering::Acquire));
    }

    #[test]
    fn a_start_that_went_through_hands_its_threads_on() {
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let r = running.clone();
        let mut threads = Starting::new(move || r.store(false, Ordering::Release));
        threads.add(worker(&running));
        let handles = threads.done();
        assert!(running.load(Ordering::Acquire), "not stopped");
        running.store(false, Ordering::Release);
        for t in handles {
            t.join().unwrap();
        }
    }

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
        let (quiet, silent) = ((Duration::from_secs(1), t), (Duration::from_secs(6), t));
        let idle = Duration::ZERO;
        assert_eq!(session_end(false, true, quiet, idle), None);
        assert_eq!(session_end(false, true, silent, idle), Some(End::TimedOut));
        assert_eq!(
            session_end(false, false, quiet, idle),
            Some(End::StreamStopped)
        );
        assert_eq!(
            session_end(true, false, quiet, idle),
            Some(End::BrowserLeft)
        );
        // A browser's packets never reach our socket: silence is normal.
        assert_eq!(session_end(true, true, silent, idle), None);
        // A slow frame is no hang; an encoder call that never returns ends
        // native and web sessions alike, and the client hears of it.
        let slow = Duration::from_millis(500);
        assert_eq!(session_end(false, true, quiet, slow), None);
        let hung = ENCODER_HANG + Duration::from_millis(1);
        for web in [false, true] {
            assert_eq!(session_end(web, true, quiet, hung), Some(End::EncoderHung));
        }
        assert!(End::EncoderHung.tells_the_client());
    }

    #[test]
    fn the_encoder_is_busy_only_inside_a_call() {
        let since = AtomicU64::new(0);
        assert_eq!(
            busy_for(since.load(Ordering::Acquire), 10_000_000),
            Duration::ZERO
        );
        {
            let _busy = Busy::enter(&since);
            let t = since.load(Ordering::Acquire);
            assert_ne!(t, 0);
            assert_eq!(busy_for(t, t + 6_000_000), Duration::from_secs(6));
            // A clock read before the call began counts as no time.
            assert_eq!(busy_for(t, t - 1), Duration::ZERO);
        }
        assert_eq!(since.load(Ordering::Acquire), 0, "idle again");
    }

    #[test]
    fn a_stuck_thread_is_left_behind_not_waited_for() {
        let release = std::sync::Arc::new(AtomicBool::new(false));
        let stuck = {
            let release = release.clone();
            std::thread::Builder::new()
                .name("encode".into())
                .spawn(move || {
                    while !release.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                })
                .unwrap()
        };
        let quick = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(20)));
        let started = Instant::now();
        assert_eq!(
            join_within(vec![stuck, quick], Duration::from_millis(200)),
            1
        );
        assert!(started.elapsed() < Duration::from_secs(2), "it waited");
        release.store(true, Ordering::Release);
        assert_eq!(join_within(Vec::new(), Duration::from_secs(1)), 0);
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
