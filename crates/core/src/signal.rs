//! Ending a program cleanly on SIGINT and SIGTERM (Ctrl+C, `systemctl
//! stop`): the signals set a stop flag the program's loop watches, so its
//! destructors run (keys released, GPU clocks restored, Bye sent).

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Turns SIGINT and SIGTERM into `stop`. A second signal ends the process
/// at once, for a loop that hangs. Call it before starting other threads:
/// they inherit the blocked signals, and only the waiting thread takes
/// them (processes started later get a clean mask from `std::process`).
#[cfg(target_os = "linux")]
pub fn stop_on_signals(stop: Arc<AtomicBool>) -> io::Result<()> {
    // SAFETY: the set is initialised by sigemptyset before use; the
    // functions only read and write the set we own.
    let set = unsafe {
        let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), libc::SIGINT);
        libc::sigaddset(set.as_mut_ptr(), libc::SIGTERM);
        set.assume_init()
    };
    // SAFETY: a valid set; the old mask is not needed.
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            loop {
                let mut sig = 0;
                // SAFETY: a valid set of blocked signals and an out-int.
                if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                    continue;
                }
                if stop.swap(true, Ordering::AcqRel) {
                    std::process::exit(128 + sig);
                }
                log::info!("signal {sig}: stopping");
            }
        })?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn stop_on_signals(_stop: Arc<AtomicBool>) -> io::Result<()> {
    Ok(())
}
