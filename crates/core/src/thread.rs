//! Hot-path threads: plain OS threads with raised scheduling priority.

use std::io;
use std::thread::{Builder, JoinHandle};

/// Nice value requested for hot-path threads. Raising priority needs
/// `CAP_SYS_NICE` or a matching `RLIMIT_NICE`; without it the thread just
/// runs at normal priority.
pub const HOT_NICE: i32 = -10;

/// Spawns a named thread and tries to raise its priority.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawns_and_joins() {
        let h = spawn_hot("test-hot", || 5).unwrap();
        assert_eq!(h.join().unwrap(), 5);
    }
}
