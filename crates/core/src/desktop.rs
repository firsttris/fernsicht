//! Who sits at the desktop.
//!
//! The host often runs as root (a system service, or sudo for KMS
//! capture), but the sound server and the desktop's settings belong to
//! the logged-in user. This finds that user: the one sudo was run by, else
//! (as root) the owner of a graphical session under `/run/user`, else the
//! current user.

use std::path::{Path, PathBuf};

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

/// What a `/run/user/<uid>` directory shows about its session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
    pub uid: u32,
    pub wayland: bool,
    pub pulse: bool,
}

/// The session to serve among those under `/run/user`: a graphical one
/// (Wayland) first, then one with sound; among equals the lowest uid of a
/// regular user (≥ 1000).
pub fn choose(sessions: &[Session]) -> Option<u32> {
    sessions
        .iter()
        .filter(|s| s.uid >= 1000 && (s.wayland || s.pulse))
        .min_by_key(|s| (!s.wayland, !s.pulse, s.uid))
        .map(|s| s.uid)
}

/// Name and home of `uid` from `/etc/passwd`'s text.
pub fn passwd_entry(passwd: &str, uid: u32) -> Option<(String, PathBuf)> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.len() >= 6 && f[2].parse::<u32>().ok()? == uid)
            .then(|| (f[0].to_owned(), PathBuf::from(f[5])))
    })
}

fn scan(run_user: &Path) -> Vec<Session> {
    std::fs::read_dir(run_user)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let uid = e.file_name().to_str()?.parse().ok()?;
            let dir = e.path();
            Some(Session {
                uid,
                wayland: dir.join("wayland-0").exists(),
                pulse: dir.join("pulse/native").exists(),
            })
        })
        .collect()
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
    choose(&scan(Path::new("/run/user"))).map(user)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(uid: u32, wayland: bool, pulse: bool) -> Session {
        Session {
            uid,
            wayland,
            pulse,
        }
    }

    #[test]
    fn the_graphical_session_wins() {
        assert_eq!(
            choose(&[s(1001, false, true), s(1000, true, true)]),
            Some(1000)
        );
        assert_eq!(
            choose(&[s(1000, false, true), s(1002, true, false)]),
            Some(1002)
        );
        assert_eq!(
            choose(&[s(1003, true, true), s(1001, true, true)]),
            Some(1001)
        );
        // System users (gdm, sddm at uid < 1000) and empty dirs don't count.
        assert_eq!(choose(&[s(42, true, true), s(1000, false, false)]), None);
        assert_eq!(choose(&[]), None);
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
    fn sessions_are_read_from_run_user() {
        let dir = std::env::temp_dir().join(format!("fernsicht-run-user-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("1000/pulse")).unwrap();
        std::fs::write(dir.join("1000/wayland-0"), "").unwrap();
        std::fs::write(dir.join("1000/pulse/native"), "").unwrap();
        std::fs::create_dir_all(dir.join("1001")).unwrap();
        std::fs::create_dir_all(dir.join("not-a-uid")).unwrap();
        let mut found = scan(&dir);
        found.sort_by_key(|s| s.uid);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, vec![s(1000, true, true), s(1001, false, false)]);
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
