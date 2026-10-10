//! The host's local control socket: pairing, status, unpairing while the
//! host runs (as a service there is no terminal to type `--pair` into).
//!
//! One JSON request per connection, one JSON answer:
//!
//! ```text
//! {"cmd":"status"}               → name, key, paired devices, session, pairing
//! {"cmd":"pair"}                 → {"ok":true,"pin":"123456","expires_in_s":300}
//! {"cmd":"stop_pairing"}         → {"ok":true}
//! {"cmd":"unpair","device":"x"}  → {"ok":true} (name or key fingerprint)
//! {"cmd":"set","gpu_boost":true} → {"ok":true} (kept in the state directory)
//! ```
//!
//! Who may ask: root, the user the host runs as, and the user at the
//! desktop (the kernel tells the caller's uid, SO_PEERCRED). The person in
//! front of the screen may pair devices; other local users may not.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::{HostSecurity, HostSettings};

/// The session as the control socket reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStatus {
    pub client: String,
    pub address: String,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub encrypted: bool,
    /// Unix time it started.
    pub since: u64,
}

/// Shared between the host's loop (writes) and the socket (reads).
pub type StatusCell = Arc<Mutex<Option<SessionStatus>>>;

/// Where the control socket goes by default: `/run/fernsicht` as root,
/// the user's runtime directory otherwise.
pub fn default_path() -> PathBuf {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return "/run/fernsicht/control.sock".into();
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    runtime.join("fernsicht/control.sock")
}

/// The control socket of the host running on this machine: the system
/// service's if it runs, else the one a host started by this user uses.
pub fn running_host_path() -> PathBuf {
    let service = PathBuf::from("/run/fernsicht/control.sock");
    if service.exists() {
        service
    } else {
        default_path()
    }
}

/// Answers one request from a caller with `uid`.
pub fn handle(
    request: &str,
    uid: u32,
    allowed: &[u32],
    sec: &HostSecurity,
    session: &StatusCell,
    settings: &HostSettings,
) -> Value {
    if !allowed.contains(&uid) {
        return json!({"ok": false, "error": "not allowed: only root and the desktop's user"});
    }
    let Ok(req) = serde_json::from_str::<Value>(request) else {
        return json!({"ok": false, "error": "not JSON"});
    };
    match req.get("cmd").and_then(Value::as_str) {
        Some("status") => {
            let paired: Vec<Value> = sec
                .paired()
                .iter()
                .map(|p| json!({"name": p.name, "key": p.key.fingerprint(), "paired_at": p.paired_at}))
                .collect();
            let session = session.lock().ok().and_then(|s| s.clone()).map(|s| {
                json!({
                    "client": s.client, "address": s.address, "width": s.width,
                    "height": s.height, "fps": s.fps, "encrypted": s.encrypted, "since": s.since,
                })
            });
            json!({
                "ok": true,
                "name": sec.name(),
                "key": sec.public_key().fingerprint(),
                "paired": paired,
                "session": session,
                "pairing": sec.pairing_remaining().map(|d| d.as_secs()),
                "gpu_boost": settings.gpu_boost(),
            })
        }
        Some("pair") => {
            let pin = fernsicht_secure::pairing::new_pin();
            sec.open_pairing(&pin);
            json!({"ok": true, "pin": pin, "expires_in_s": crate::PAIRING_OPEN_FOR.as_secs()})
        }
        Some("set") => match req.get("gpu_boost").and_then(Value::as_bool) {
            Some(on) => match settings.set_gpu_boost(on) {
                Ok(()) => json!({"ok": true}),
                Err(e) => json!({"ok": false, "error": format!("saving the setting: {e}")}),
            },
            None => json!({"ok": false, "error": "which setting?"}),
        },
        Some("stop_pairing") => {
            sec.close_pairing();
            json!({"ok": true})
        }
        Some("unpair") => match req.get("device").and_then(Value::as_str) {
            Some(d) if sec.unpair(d) => json!({"ok": true}),
            Some(d) => json!({"ok": false, "error": format!("no paired device {d}")}),
            None => json!({"ok": false, "error": "which device?"}),
        },
        _ => json!({"ok": false, "error": "unknown command"}),
    }
}

/// The caller's uid (SO_PEERCRED).
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid socket and out-pointers of the right size.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(cred.uid)
}

/// Who may use the socket: root, us, and the desktop's user.
pub fn allowed_uids() -> Vec<u32> {
    // SAFETY: geteuid has no preconditions.
    let mut uids = vec![0, unsafe { libc::geteuid() }];
    if let Some(u) = fernsicht_core::desktop::desktop_user() {
        uids.push(u.uid);
    }
    uids
}

/// Serves the socket at `path` until `stop`. Replaces a stale socket.
pub fn serve(
    path: &Path,
    sec: Arc<HostSecurity>,
    session: StatusCell,
    settings: Arc<HostSettings>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // Anyone may connect; the uid check decides (a root service's socket
    // must be reachable for the desktop's user).
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    listener.set_nonblocking(true)?;
    let path = path.to_owned();
    std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let allowed = allowed_uids();
                        answer(stream, &allowed, &sec, &session, &settings);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        log::warn!("control socket: {e}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
            let _ = std::fs::remove_file(&path);
        })
}

fn answer(
    stream: UnixStream,
    allowed: &[u32],
    sec: &HostSecurity,
    session: &StatusCell,
    settings: &HostSettings,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let Some(uid) = peer_uid(&stream) else { return };
    let mut line = String::new();
    let mut reader = BufReader::new(&stream);
    if reader.by_ref().take(4096).read_line(&mut line).is_err() {
        return;
    }
    let reply = handle(line.trim(), uid, allowed, sec, session, settings);
    let mut w = &stream;
    let _ = writeln!(w, "{reply}");
}

/// Sends one request to a host's control socket and returns the answer.
pub fn request(path: &Path, req: &Value) -> anyhow::Result<Value> {
    use anyhow::Context;
    let stream = UnixStream::connect(path)
        .with_context(|| format!("no host at {} (is the host running?)", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut w = &stream;
    writeln!(w, "{req}")?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let v: Value = serde_json::from_str(line.trim()).context("odd answer from the host")?;
    if v.get("ok") != Some(&Value::Bool(true)) {
        anyhow::bail!(
            "{}",
            v.get("error").and_then(Value::as_str).unwrap_or("failed")
        );
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fernsicht_secure::{Identity, Peer, Trusted};

    fn sec() -> HostSecurity {
        let mut t = Trusted::default();
        t.add(Peer {
            name: "bazzite".into(),
            key: Identity::generate().public,
            address: None,
            paired_at: 5,
        });
        HostSecurity::new(Identity::generate(), "zentrale", t, None)
    }

    #[test]
    fn status_pair_and_unpair() {
        let s = sec();
        let cell: StatusCell = Arc::default();
        let st = handle(
            r#"{"cmd":"status"}"#,
            1000,
            &[1000],
            &s,
            &cell,
            &HostSettings::default(),
        );
        assert_eq!(st["ok"], true);
        assert_eq!(st["name"], "zentrale");
        assert_eq!(st["paired"][0]["name"], "bazzite");
        assert_eq!(st["session"], Value::Null);
        assert_eq!(st["pairing"], Value::Null);

        let p = handle(
            r#"{"cmd":"pair"}"#,
            1000,
            &[1000],
            &s,
            &cell,
            &HostSettings::default(),
        );
        assert_eq!(p["pin"].as_str().unwrap().len(), 6);
        assert!(
            handle(
                r#"{"cmd":"status"}"#,
                1000,
                &[1000],
                &s,
                &cell,
                &HostSettings::default()
            )["pairing"]
                .as_u64()
                .unwrap()
                > 290
        );
        handle(
            r#"{"cmd":"stop_pairing"}"#,
            1000,
            &[1000],
            &s,
            &cell,
            &HostSettings::default(),
        );
        assert_eq!(
            handle(
                r#"{"cmd":"status"}"#,
                1000,
                &[1000],
                &s,
                &cell,
                &HostSettings::default()
            )["pairing"],
            Value::Null
        );

        *cell.lock().unwrap() = Some(SessionStatus {
            client: "bazzite".into(),
            address: "192.168.178.20:5000".into(),
            width: 2560,
            height: 1440,
            fps: 60,
            encrypted: true,
            since: 7,
        });
        let st = handle(
            r#"{"cmd":"status"}"#,
            0,
            &[0],
            &s,
            &cell,
            &HostSettings::default(),
        );
        assert_eq!(st["session"]["width"], 2560);

        let u = handle(
            r#"{"cmd":"unpair","device":"bazzite"}"#,
            1000,
            &[1000],
            &s,
            &cell,
            &HostSettings::default(),
        );
        assert_eq!(u["ok"], true);
        assert!(s.paired().is_empty());
        let again = handle(
            r#"{"cmd":"unpair","device":"bazzite"}"#,
            1000,
            &[1000],
            &s,
            &cell,
            &HostSettings::default(),
        );
        assert_eq!(again["ok"], false);
    }

    #[test]
    fn the_gpu_boost_setting_is_kept() {
        let s = sec();
        let cell: StatusCell = Arc::default();
        let dir = std::env::temp_dir().join(format!("fernsicht-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("settings.json");
        let set = HostSettings::load(file.clone());
        assert!(set.gpu_boost(), "on unless switched off");
        let st = handle(r#"{"cmd":"status"}"#, 1000, &[1000], &s, &cell, &set);
        assert_eq!(st["gpu_boost"], true);
        let r = handle(
            r#"{"cmd":"set","gpu_boost":false}"#,
            1000,
            &[1000],
            &s,
            &cell,
            &set,
        );
        assert_eq!(r["ok"], true);
        assert!(!set.gpu_boost());
        assert!(
            !HostSettings::load(file.clone()).gpu_boost(),
            "kept on disk"
        );
        let bad = handle(r#"{"cmd":"set"}"#, 1000, &[1000], &s, &cell, &set);
        assert_eq!(bad["ok"], false);
        // A stranger may not change it.
        let r = handle(
            r#"{"cmd":"set","gpu_boost":true}"#,
            1001,
            &[1000],
            &s,
            &cell,
            &set,
        );
        assert_eq!(r["ok"], false);
        assert!(!set.gpu_boost());
        // A broken file: the default.
        std::fs::write(&file, "{nonsense").unwrap();
        assert!(HostSettings::load(file).gpu_boost());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strangers_and_nonsense_are_turned_away() {
        let s = sec();
        let cell: StatusCell = Arc::default();
        let r = handle(
            r#"{"cmd":"pair"}"#,
            1001,
            &[0, 1000],
            &s,
            &cell,
            &HostSettings::default(),
        );
        assert_eq!(r["ok"], false);
        assert!(s.pairing_remaining().is_none(), "no pairing for strangers");
        assert_eq!(
            handle("hello", 1000, &[1000], &s, &cell, &HostSettings::default())["ok"],
            false
        );
        assert_eq!(
            handle(
                r#"{"cmd":"rm -rf"}"#,
                1000,
                &[1000],
                &s,
                &cell,
                &HostSettings::default()
            )["ok"],
            false
        );
        assert_eq!(
            handle(
                r#"{"cmd":"unpair"}"#,
                1000,
                &[1000],
                &s,
                &cell,
                &HostSettings::default()
            )["ok"],
            false
        );
    }

    #[test]
    fn the_socket_serves_requests() {
        let dir = std::env::temp_dir().join(format!("fernsicht-control-{}", std::process::id()));
        let path = dir.join("control.sock");
        let s = Arc::new(sec());
        let stop = Arc::new(AtomicBool::new(false));
        let t = serve(
            &path,
            s.clone(),
            Arc::default(),
            Arc::default(),
            stop.clone(),
        )
        .unwrap();
        let st = request(&path, &json!({"cmd": "status"})).unwrap();
        assert_eq!(st["name"], "zentrale");
        let pin = request(&path, &json!({"cmd": "pair"})).unwrap()["pin"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(pin.len(), 6);
        let e = request(&path, &json!({"cmd": "nope"}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown command"), "{e}");
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert!(!path.exists(), "socket removed on exit");
        let e = request(&path, &json!({"cmd": "status"}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("is the host running"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
