//! The host's local control socket: pairing, status, unpairing while the
//! host runs (as a service there is no terminal to type `--pair` into).
//!
//! One JSON request per connection, one JSON answer:
//!
//! ```text
//! {"cmd":"status"}               → name, key, paired devices, session, pairing
//! {"cmd":"pair"}                 → {"ok":true,"pin":"123456","expires_in_s":300}
//! {"cmd":"stop_pairing"}         → {"ok":true}
//! {"cmd":"unpair","device":"x"}  → {"ok":true} (name or key fingerprint;
//!                                  ends that device's session)
//! {"cmd":"disconnect"}           → {"ok":true} (ends the running session)
//! {"cmd":"set","gpu_boost":true} → {"ok":true} (kept in the state directory)
//! {"cmd":"set","rendezvous":"https://…"} → {"ok":true} (the rendezvous
//!                                  service; "" for none; kept too)
//! {"cmd":"set","port_mapping":true} → {"ok":true} (ask the router to
//!                                  forward the port; kept too)
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

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{HostSecurity, HostSettings};

/// A request on the control socket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Pair,
    StopPairing,
    Disconnect,
    /// By name or key fingerprint.
    Unpair {
        device: Option<String>,
    },
    Set {
        #[serde(default)]
        gpu_boost: Option<bool>,
        /// The rendezvous service's address; empty: none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rendezvous: Option<String>,
        /// Ask the router to forward the host's port.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port_mapping: Option<bool>,
    },
}

/// The answer to `status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Status {
    pub name: String,
    /// The host key's fingerprint.
    pub key: String,
    pub paired: Vec<PairedDevice>,
    pub session: Option<SessionStatus>,
    /// Seconds pairing stays open, if it is.
    pub pairing: Option<u64>,
    /// The GPU is clocked up during sessions.
    pub gpu_boost: bool,
    /// How the host stands with its rendezvous service (connections over
    /// the internet).
    #[serde(default)]
    pub internet: crate::internet::InternetStatus,
}

/// A device paired with the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PairedDevice {
    pub name: String,
    /// Its key's fingerprint.
    pub key: String,
    /// Unix time it paired.
    pub paired_at: u64,
}

/// The answer to `pair`: the PIN the host shows now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct PairingOpened {
    pub pin: String,
    pub expires_in_s: u64,
}

/// The session as the control socket reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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

/// What the host's loop and its threads report, for the socket to read.
#[derive(Clone, Debug, Default)]
pub struct Reports {
    pub session: StatusCell,
    pub internet: crate::internet::InternetCell,
}

/// Where the control socket goes by default: `/run/fernsicht` as root,
/// the user's runtime directory otherwise.
pub fn default_path() -> PathBuf {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return "/run/fernsicht/control.sock".into();
    }
    // Not /tmp without it: another user could create the directory there
    // first. The runtime directory is the user's own.
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        // SAFETY: getuid has no preconditions.
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
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
    reports: &Reports,
    settings: &HostSettings,
) -> Value {
    if !allowed.contains(&uid) {
        return failed("not allowed: only root and the desktop's user");
    }
    let Ok(req) = serde_json::from_str::<Value>(request) else {
        return failed("not JSON");
    };
    let Ok(req) = serde_json::from_value::<Request>(req) else {
        return failed("unknown command");
    };
    match req {
        Request::Status => ok(Status {
            name: sec.name().to_owned(),
            key: sec.public_key().fingerprint(),
            paired: sec
                .paired()
                .iter()
                .map(|p| PairedDevice {
                    name: p.name.clone(),
                    key: p.key.fingerprint(),
                    paired_at: p.paired_at,
                })
                .collect(),
            session: reports.session.lock().ok().and_then(|s| s.clone()),
            pairing: sec.pairing_remaining().map(|d| d.as_secs()),
            gpu_boost: settings.gpu_boost(),
            internet: reports
                .internet
                .lock()
                .map(|i| i.clone())
                .unwrap_or_default(),
        }),
        Request::Pair => {
            let pin = fernsicht_secure::pairing::new_pin();
            sec.open_pairing(&pin);
            ok(PairingOpened {
                pin,
                expires_in_s: crate::PAIRING_OPEN_FOR.as_secs(),
            })
        }
        Request::Set {
            gpu_boost: Some(on),
            rendezvous: None,
            port_mapping: None,
        } => match settings.set_gpu_boost(on) {
            Ok(()) => done(),
            Err(e) => failed(&format!("saving the setting: {e}")),
        },
        Request::Set {
            gpu_boost: None,
            rendezvous: None,
            port_mapping: Some(on),
        } => match settings.set_port_mapping(on) {
            Ok(()) => done(),
            Err(e) => failed(&format!("saving the setting: {e}")),
        },
        Request::Set {
            gpu_boost: None,
            rendezvous: Some(url),
            port_mapping: None,
        } => {
            // Empty: none (LAN and VPN only).
            let url = Some(url.trim().to_owned()).filter(|u| !u.is_empty());
            match settings.set_rendezvous(url) {
                Ok(()) => done(),
                Err(e) => failed(&e.to_string()),
            }
        }
        Request::Set { .. } => failed("one setting at a time"),
        Request::StopPairing => {
            sec.close_pairing();
            done()
        }
        Request::Disconnect => {
            sec.end_session();
            done()
        }
        Request::Unpair { device: Some(d) } => match sec.unpair(&d) {
            Ok(()) => done(),
            Err(e) => failed(&format!("{d}: {e}")),
        },
        Request::Unpair { device: None } => failed("which device?"),
    }
}

/// `{"ok": true}` with the answer's fields.
fn ok(answer: impl Serialize) -> Value {
    let mut v = serde_json::to_value(answer).expect("plain data");
    v["ok"] = Value::Bool(true);
    v
}

fn done() -> Value {
    json!({"ok": true})
}

fn failed(error: &str) -> Value {
    json!({"ok": false, "error": error})
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
    reports: Reports,
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
                        answer(stream, &allowed, &sec, &reports, &settings, REQUEST_TIME);
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

/// How long a caller has for its request, in total: one connection at a
/// time is served, so a slow one must not hold up the others.
const REQUEST_TIME: Duration = Duration::from_secs(2);
/// Longest request line.
const MAX_REQUEST: usize = 4096;

fn answer(
    stream: UnixStream,
    allowed: &[u32],
    sec: &HostSecurity,
    reports: &Reports,
    settings: &HostSettings,
    within: Duration,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(within));
    let Some(uid) = peer_uid(&stream) else { return };
    let mut w = &stream;
    // Who it is comes first: a caller who may not ask is answered without
    // reading anything.
    if !allowed.contains(&uid) {
        let _ = writeln!(w, "{}", handle("", uid, allowed, sec, reports, settings));
        return;
    }
    let Some(line) = read_line_within(&stream, within) else {
        return;
    };
    let reply = handle(line.trim(), uid, allowed, sec, reports, settings);
    let _ = writeln!(w, "{reply}");
}

/// One line of at most [`MAX_REQUEST`] bytes, within `within` in total
/// (not per read: a caller dripping a byte at a time is cut off too).
fn read_line_within(mut stream: &UnixStream, within: Duration) -> Option<String> {
    let deadline = std::time::Instant::now() + within;
    let mut buf = Vec::new();
    let mut byte = [0u8; 256];
    while !buf.contains(&b'\n') {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || buf.len() > MAX_REQUEST || stream.set_read_timeout(Some(left)).is_err()
        {
            return None;
        }
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&byte[..n]),
            Err(_) => return None,
        }
    }
    let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

/// Asks the host at `path` and reads its answer as `T`.
pub fn ask<T: DeserializeOwned>(path: &Path, req: &Request) -> anyhow::Result<T> {
    use anyhow::Context;
    serde_json::from_value(request(path, req)?).context("odd answer from the host")
}

/// Sends one request to a host's control socket and returns the answer.
pub fn request(path: &Path, req: &impl Serialize) -> anyhow::Result<Value> {
    use anyhow::Context;
    let stream = UnixStream::connect(path)
        .with_context(|| format!("no host at {} (is the host running?)", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut w = &stream;
    writeln!(w, "{}", serde_json::to_string(req)?)?;
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
            rendezvous: None,
        });
        HostSecurity::new(Identity::generate(), "zentrale", t, None)
    }

    #[test]
    fn a_caller_who_may_not_ask_is_turned_away_without_reading() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        // Our own uid is not in the list; nothing is ever sent.
        let started = std::time::Instant::now();
        answer(
            ours,
            &[],
            &sec(),
            &Reports::default(),
            &HostSettings::default(),
            Duration::from_secs(5),
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        let mut reply = String::new();
        BufReader::new(&theirs).read_line(&mut reply).unwrap();
        assert!(reply.contains("not allowed"), "{reply}");
    }

    #[test]
    fn a_dripping_caller_is_cut_off_in_time() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        let dripper = std::thread::spawn(move || {
            let mut w = &theirs;
            for b in br#"{"cmd":"status"}"# {
                if w.write_all(&[*b]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let started = std::time::Instant::now();
        answer(
            ours,
            &[me],
            &sec(),
            &Reports::default(),
            &HostSettings::default(),
            Duration::from_millis(200),
        );
        assert!(started.elapsed() < Duration::from_millis(600));
        dripper.join().unwrap();
    }

    #[test]
    fn disconnect_asks_the_host_to_end_the_session() {
        let s = sec();
        let r = handle(
            r#"{"cmd":"disconnect"}"#,
            1000,
            &[1000],
            &s,
            &Reports::default(),
            &HostSettings::default(),
        );
        assert_eq!(r["ok"], true);
        assert!(s.take_end_request());
        assert!(!s.take_end_request(), "once");
    }

    #[test]
    fn status_pair_and_unpair() {
        let s = sec();
        let cell = Reports::default();
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

        *cell.session.lock().unwrap() = Some(SessionStatus {
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
        let cell = Reports::default();
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
    fn the_rendezvous_service_is_set_checked_and_kept() {
        let s = sec();
        let cell = Reports::default();
        let dir = std::env::temp_dir().join(format!("fernsicht-rendezvous-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("settings.json");
        let set = HostSettings::load(file.clone());
        let ask = |req: &str| handle(req, 1000, &[1000], &s, &cell, &set);
        assert_eq!(ask(r#"{"cmd":"status"}"#)["internet"]["state"], "off");
        let r = ask(r#"{"cmd":"set","rendezvous":"https://r.someone.workers.dev/"}"#);
        assert_eq!(r["ok"], true);
        assert_eq!(
            set.rendezvous().as_deref(),
            Some("https://r.someone.workers.dev")
        );
        let back = HostSettings::load(file.clone());
        assert_eq!(back.rendezvous(), set.rendezvous(), "kept on disk");
        assert!(back.gpu_boost(), "the other setting stays");
        // Not an address: refused, the old one stays.
        assert_eq!(ask(r#"{"cmd":"set","rendezvous":"ftp://x"}"#)["ok"], false);
        assert!(set.rendezvous().is_some());
        // Both at once: one at a time.
        let both = r#"{"cmd":"set","gpu_boost":true,"rendezvous":""}"#;
        assert_eq!(ask(both)["ok"], false);
        // Empty: none.
        assert_eq!(ask(r#"{"cmd":"set","rendezvous":""}"#)["ok"], true);
        assert_eq!(set.rendezvous(), None);
        assert_eq!(HostSettings::load(file).rendezvous(), None);
        // What the signaling thread reports shows in the status.
        *cell.internet.lock().unwrap() = crate::internet::InternetStatus {
            url: Some("https://x.example".into()),
            state: crate::internet::InternetState::Registered,
            relay: true,
            error: None,
            forwarding: Default::default(),
        };
        let st = ask(r#"{"cmd":"status"}"#);
        assert_eq!(st["internet"]["state"], "registered");
        assert_eq!(st["internet"]["relay"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strangers_and_nonsense_are_turned_away() {
        let s = sec();
        let cell = Reports::default();
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
            Reports::default(),
            Arc::default(),
            stop.clone(),
        )
        .unwrap();
        let st: Status = ask(&path, &Request::Status).unwrap();
        assert_eq!(st.name, "zentrale");
        assert_eq!(st.paired[0].name, "bazzite");
        let opened: PairingOpened = ask(&path, &Request::Pair).unwrap();
        assert_eq!(opened.pin.len(), 6);
        let e = request(&path, &json!({"cmd": "nope"}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown command"), "{e}");
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert!(!path.exists(), "socket removed on exit");
        let e = request(&path, &Request::Status).unwrap_err().to_string();
        assert!(e.contains("is the host running"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
