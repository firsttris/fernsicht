//! Who sits at the desktop.
//!
//! The host often runs as root (a system service, or sudo for KMS
//! capture), but the sound server and the desktop's settings belong to
//! the logged-in user. This finds that user: the one sudo was run by, else
//! (as root) the user of the active session on the machine's seat, as
//! logind says (`sd_seat_get_active`), else the current user.
//!
//! Not files under `/run/user`: every user owns their own directory
//! there and could pretend to have a desktop (the control socket lets the
//! desktop's user pair devices).

use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopUser {
    pub uid: u32,
    pub name: String,
    pub home: PathBuf,
}

impl DesktopUser {
    /// The user's runtime directory (sockets of the session).
    pub fn runtime_dir(&self) -> PathBuf {
        PathBuf::from(format!("/run/user/{}", self.uid))
    }
}

/// The desktop's user among what logind reports: a regular user (not root
/// nor a system account such as the login screen's).
pub fn regular_user(uid: Option<u32>) -> Option<u32> {
    uid.filter(|&u| u >= 1000)
}

/// Name and home of `uid` from `/etc/passwd`'s text.
pub fn passwd_entry(passwd: &str, uid: u32) -> Option<(String, PathBuf)> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.len() >= 6 && f[2].parse::<u32>().ok()? == uid)
            .then(|| (f[0].to_owned(), PathBuf::from(f[5])))
    })
}

/// The user of the active session on seat0, from logind
/// (`sd_seat_get_active` in libsystemd, loaded at runtime); `None` without
/// logind or an active session.
fn active_seat_uid() -> Option<u32> {
    type SeatGetActive = unsafe extern "C" fn(
        seat: *const std::ffi::c_char,
        session: *mut *mut std::ffi::c_char,
        uid: *mut libc::uid_t,
    ) -> std::ffi::c_int;
    // SAFETY: loading libsystemd runs no code of ours; the symbol has the
    // signature of sd-login.h. The session may be NULL (only the uid is
    // asked for), as sd-login.c allows.
    unsafe {
        let lib = libloading::Library::new("libsystemd.so.0").ok()?;
        let get: libloading::Symbol<SeatGetActive> = lib.get(b"sd_seat_get_active\0").ok()?;
        let mut uid: libc::uid_t = 0;
        (get(c"seat0".as_ptr(), std::ptr::null_mut(), &mut uid) >= 0).then_some(uid)
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn user(uid: u32) -> DesktopUser {
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let (name, home) = passwd_entry(&passwd, uid)
        .unwrap_or_else(|| (uid.to_string(), PathBuf::from(format!("/home/{uid}"))));
    DesktopUser { uid, name, home }
}

/// The desktop's user; `None` if there is none (a headless root host).
pub fn desktop_user() -> Option<DesktopUser> {
    if let Some(uid) = std::env::var("SUDO_UID").ok().and_then(|u| u.parse().ok())
        && uid != 0
    {
        return Some(user(uid));
    }
    let me = effective_uid();
    if me != 0 {
        let mut u = user(me);
        if let Some(home) = std::env::var_os("HOME") {
            u.home = home.into();
        }
        return Some(u);
    }
    regular_user(active_seat_uid()).map(user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_regular_users_sit_at_the_desktop() {
        assert_eq!(regular_user(Some(1000)), Some(1000));
        // Root on a console, the login screen's account, nobody active.
        assert_eq!(regular_user(Some(0)), None);
        assert_eq!(regular_user(Some(980)), None);
        assert_eq!(regular_user(None), None);
    }

    #[test]
    fn logind_names_the_active_user_if_there_is_one() {
        // Compared with logind's own state file where there is one (the
        // test may read it; the code uses the public API). In a container
        // without a seat there is no active user.
        let expected = std::fs::read_to_string("/run/systemd/seats/seat0")
            .ok()
            .and_then(|t| {
                t.lines()
                    .find_map(|l| l.strip_prefix("ACTIVE_UID=")?.parse::<u32>().ok())
            });
        if expected.is_some() {
            assert_eq!(active_seat_uid(), expected);
        }
    }

    #[test]
    fn passwd_lookup() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      tristan:x:1000:1000:Tristan:/home/tristan:/bin/sh\n\
                      broken line\n";
        assert_eq!(
            passwd_entry(passwd, 1000),
            Some(("tristan".into(), PathBuf::from("/home/tristan")))
        );
        assert_eq!(passwd_entry(passwd, 0).unwrap().0, "root");
        assert_eq!(passwd_entry(passwd, 1001), None);
    }

    #[test]
    fn there_is_a_desktop_user_when_not_root() {
        if effective_uid() == 0 {
            return;
        }
        let u = desktop_user().unwrap();
        assert_eq!(u.uid, effective_uid());
        assert!(u.runtime_dir().ends_with(u.uid.to_string()));
    }
}
