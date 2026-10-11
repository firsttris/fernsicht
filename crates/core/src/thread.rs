//! Hot-path threads: plain OS threads with raised scheduling priority and
//! precise sleeps.

use std::io;
use std::thread::{Builder, JoinHandle};

/// Nice value requested for hot-path threads. Raising priority needs
/// `CAP_SYS_NICE` or a matching `RLIMIT_NICE`; without it the thread just
/// runs at normal priority.
pub const HOT_NICE: i32 = -10;

/// How late a hot thread's sleep may end: Linux defaults to 50 µs, which a
/// pacer sleeping between packets would otherwise spin away.
pub const HOT_TIMER_SLACK_NS: u64 = 1_000;

/// Spawns a named thread, tries to raise its priority and makes its sleeps
/// precise ([`HOT_TIMER_SLACK_NS`]).
pub fn spawn_hot<F, T>(name: &str, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let thread_name = name.to_owned();
    Builder::new().name(thread_name.clone()).spawn(move || {
        if let Err(err) = raise_priority(HOT_NICE) {
            log::debug!("{thread_name}: could not raise priority: {err}");
        }
        if let Err(err) = set_timer_slack(HOT_TIMER_SLACK_NS) {
            log::debug!("{thread_name}: could not set the timer slack: {err}");
        }
        f()
    })
}

/// Sets the nice value of the calling thread.
#[cfg(target_os = "linux")]
pub fn raise_priority(nice: i32) -> io::Result<()> {
    // SAFETY: gettid has no preconditions; setpriority only reads its
    // arguments. On Linux, PRIO_PROCESS with a thread id targets that thread.
    let rc = unsafe {
        let tid = libc::gettid();
        libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, nice)
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
pub fn raise_priority(_nice: i32) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "linux only"))
}

/// Sets how late the calling thread's timers may fire, nanoseconds.
#[cfg(target_os = "linux")]
pub fn set_timer_slack(ns: u64) -> io::Result<()> {
    // SAFETY: PR_SET_TIMERSLACK takes the slack as an unsigned long and
    // affects only the calling thread.
    let rc = unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, ns as libc::c_ulong, 0, 0, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_timer_slack(_ns: u64) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "linux only"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawns_and_joins() {
        let h = spawn_hot("test-hot", || 5).unwrap();
        assert_eq!(h.join().unwrap(), 5);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn hot_threads_sleep_precisely() {
        let slack = spawn_hot("test-slack", || {
            // SAFETY: PR_GET_TIMERSLACK returns the calling thread's slack.
            unsafe { libc::prctl(libc::PR_GET_TIMERSLACK, 0, 0, 0, 0) }
        })
        .unwrap();
        assert_eq!(slack.join().unwrap() as u64, HOT_TIMER_SLACK_NS);
    }
}
