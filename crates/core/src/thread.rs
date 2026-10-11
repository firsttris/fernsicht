//! Hot-path threads: plain OS threads with real-time or raised scheduling
//! priority and precise sleeps. Each of them blocks regularly (on a vblank,
//! the GPU, a queue, the network), so a real-time one cannot starve the
//! machine, and the kernel's real-time throttling stays as the net.

use std::io;
use std::thread::{Builder, JoinHandle};

/// Real-time priority (`SCHED_FIFO`) for hot threads where allowed (root,
/// `CAP_SYS_NICE`, an `RLIMIT_RTPRIO`): low, so they run before every normal
/// thread but after audio servers (PipeWire asks for 88) and the kernel's.
pub const HOT_RT_PRIORITY: i32 = 5;

/// Nice value requested for hot-path threads where real time is not
/// allowed. Needs `CAP_SYS_NICE` or a matching `RLIMIT_NICE`; without it the
/// thread runs at normal priority.
pub const HOT_NICE: i32 = -10;

/// How a hot thread is scheduled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// `SCHED_FIFO` at [`HOT_RT_PRIORITY`].
    Realtime,
    /// Normal scheduling at [`HOT_NICE`].
    Raised,
    /// Neither was allowed.
    Normal,
}

/// How late a hot thread's sleep may end: Linux defaults to 50 µs, which a
/// pacer sleeping between packets would otherwise spin away.
pub const HOT_TIMER_SLACK_NS: u64 = 1_000;

/// Spawns a named hot thread (see [`make_current_hot`]).
pub fn spawn_hot<F, T>(name: &str, f: F) -> io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let thread_name = name.to_owned();
    Builder::new().name(thread_name.clone()).spawn(move || {
        make_current_hot(&thread_name);
        f()
    })
}

/// Makes the calling thread hot: real-time where allowed, else a higher
/// nice value; and precise sleeps ([`HOT_TIMER_SLACK_NS`]).
pub fn make_current_hot(name: &str) -> Priority {
    let priority = match set_realtime(HOT_RT_PRIORITY) {
        Ok(()) => Priority::Realtime,
        Err(rt) => match raise_priority(HOT_NICE) {
            Ok(()) => Priority::Raised,
            Err(nice) => {
                log::debug!("{name}: normal priority (real time: {rt}; nice: {nice})");
                Priority::Normal
            }
        },
    };
    if let Err(err) = set_timer_slack(HOT_TIMER_SLACK_NS) {
        log::debug!("{name}: could not set the timer slack: {err}");
    }
    log::debug!("{name}: {priority:?}");
    priority
}

/// Schedules the calling thread `SCHED_FIFO` at `priority`. Threads and
/// processes it starts do not inherit it.
#[cfg(target_os = "linux")]
pub fn set_realtime(priority: i32) -> io::Result<()> {
    let param = libc::sched_param {
        sched_priority: priority,
    };
    // SAFETY: pid 0 is the calling thread; the parameter is read only.
    let rc = unsafe {
        libc::sched_setscheduler(0, libc::SCHED_FIFO | libc::SCHED_RESET_ON_FORK, &param)
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
pub fn set_realtime(_priority: i32) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "linux only"))
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
    fn a_hot_thread_is_what_it_says() {
        let (said, policy, nice) = spawn_hot("test-priority", || {
            let said = make_current_hot("test-priority");
            // SAFETY: plain queries of the calling thread.
            let (policy, nice) = unsafe {
                (
                    libc::sched_getscheduler(0) & !libc::SCHED_RESET_ON_FORK,
                    libc::getpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t),
                )
            };
            (said, policy, nice)
        })
        .unwrap()
        .join()
        .unwrap();
        match said {
            Priority::Realtime => assert_eq!(policy, libc::SCHED_FIFO),
            Priority::Raised => assert_eq!((policy, nice), (libc::SCHED_OTHER, HOT_NICE)),
            Priority::Normal => assert_eq!(policy, libc::SCHED_OTHER),
        }
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
