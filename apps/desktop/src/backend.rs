//! What the app's commands do, without Tauri (so it is testable): devices
//! from LAN discovery and the paired list, pairing, sessions as child
//! processes of the native client, and the host on this machine.
//!
//! State lives where the command-line client keeps it (`~/.config/fernsicht`),
//! so pairings made in a terminal show up in the app and the other way
//! round.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use fernsicht_client::discover::{FoundHost, broadcast_targets, discover};
use fernsicht_client::{DEFAULT_PORT, Identity, Trusted, pair, refresh_addresses};
use fernsicht_host_agent::control;
use serde::Serialize;
use serde_json::{Value, json};

/// A device as the UI's `Device` type has it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    /// The host key's fingerprint.
    pub id: String,
    pub name: String,
    pub online: bool,
    pub favorite: bool,
    pub os: String,
    pub gpu: String,
    pub paired: bool,
    /// Pairing is open there.
    pub pairing: bool,
    /// Someone is connected.
    pub busy: bool,
    pub address: Option<String>,
}

/// The session as the UI polls it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    /// The client still runs.
    pub active: bool,
    pub device_id: Option<String>,
    pub device_name: Option<String>,
    /// The client's latest overlay (see `fernsicht_render::overlay::json`).
    pub stats: Option<Value>,
    /// Why it ended, if it failed.
    pub error: Option<String>,
}

/// This computer: its name and, if a host runs here, the host's status
/// (the control socket's answer).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThisMachine {
    pub name: String,
    pub host: Option<Value>,
}

/// A running client process.
struct Running {
    device_id: String,
    device_name: String,
    child: Child,
    /// Closing it ends the client (it runs with `--app`).
    stdin: Option<ChildStdin>,
    stats: Arc<Mutex<Option<Value>>>,
    log: Arc<Mutex<VecDeque<String>>>,
}

/// stderr lines kept for error messages.
const LOG_LINES: usize = 40;

impl Running {
    /// Asks the client to stop, then makes sure it does.
    fn stop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// The line that best says why the client failed.
    fn error(&self) -> String {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.iter()
            .rev()
            .find_map(|l| l.strip_prefix("Error: "))
            .or_else(|| {
                log.iter()
                    .rev()
                    .find(|l| l.contains("ERROR"))
                    .map(String::as_str)
            })
            .or_else(|| log.back().map(String::as_str))
            .unwrap_or("the client ended unexpectedly")
            .to_owned()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct Backend {
    /// Key and paired hosts.
    pub dir: PathBuf,
    /// The `fernsicht-client` program.
    pub client: PathBuf,
    /// Extra arguments for it (tests: `--headless`).
    pub client_args: Vec<String>,
    /// The local host's control socket; `None`: the usual places.
    pub control: Option<PathBuf>,
    /// Asked besides the broadcast addresses (tests: localhost).
    pub extra_targets: Vec<SocketAddr>,
    /// How long discovery waits for answers.
    pub discovery_wait: Duration,
    /// This device's name for hosts it pairs with.
    pub name: String,
    session: Mutex<Option<Running>>,
}

/// The machine's hostname.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "fernsicht".into())
}

/// The client program: `$FERNSICHT_CLIENT`, else next to this program,
/// else from `PATH`.
pub fn find_client() -> PathBuf {
    if let Some(p) = std::env::var_os("FERNSICHT_CLIENT") {
        return p.into();
    }
    std::env::current_exe()
        .map(|e| e.with_file_name("fernsicht-client"))
        .ok()
        .filter(|p| p.exists())
        .unwrap_or_else(|| "fernsicht-client".into())
}

impl Backend {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            client: find_client(),
            client_args: Vec::new(),
            control: None,
            extra_targets: Vec::new(),
            discovery_wait: Duration::from_millis(800),
            name: hostname(),
            session: Mutex::new(None),
        }
    }

    fn hosts_path(&self) -> PathBuf {
        self.dir.join("hosts.json")
    }

    fn trusted(&self) -> anyhow::Result<Trusted> {
        Trusted::load(&self.hosts_path()).map_err(anyhow::Error::msg)
    }

    fn save(&self, t: &Trusted) -> anyhow::Result<()> {
        t.save(&self.hosts_path()).map_err(anyhow::Error::msg)
    }

    /// Paired hosts and hosts in the LAN: paired ones first, then by name.
    /// Paired hosts that moved get their new address.
    pub fn devices(&self) -> anyhow::Result<Vec<Device>> {
        let mut trusted = self.trusted()?;
        let mut targets = broadcast_targets(DEFAULT_PORT);
        targets.extend(self.extra_targets.iter().copied());
        targets.extend(
            trusted
                .peers
                .iter()
                .filter_map(|p| p.address.as_deref()?.parse::<SocketAddr>().ok()),
        );
        let found = discover(&targets, self.discovery_wait).context("looking for hosts")?;
        if refresh_addresses(&mut trusted, &found) {
            self.save(&trusted)?;
        }
        Ok(merge(&trusted, &found))
    }

    /// Pairs with the host at `address` (or of that name in the LAN) with
    /// the PIN it shows.
    pub fn pair(&self, address: &str, pin: &str) -> anyhow::Result<Device> {
        let identity =
            Identity::load_or_create(&self.dir.join("client.json")).map_err(anyhow::Error::msg)?;
        let addr = fernsicht_client::with_default_port(address.trim());
        let peer = pair(&addr, pin, &identity, &self.name)?;
        let mut trusted = self.trusted()?;
        trusted.add(peer.clone());
        self.save(&trusted)?;
        Ok(Device {
            id: peer.key.fingerprint(),
            name: peer.name,
            online: true,
            favorite: false,
            os: String::new(),
            gpu: String::new(),
            paired: true,
            pairing: false,
            busy: false,
            address: peer.address,
        })
    }

    /// Forgets a paired host.
    pub fn forget(&self, id: &str) -> anyhow::Result<()> {
        let mut trusted = self.trusted()?;
        let key = trusted
            .peers
            .iter()
            .find(|p| p.key.fingerprint() == id)
            .map(|p| p.key)
            .with_context(|| format!("no paired device {id}"))?;
        trusted.remove(&key);
        self.save(&trusted)
    }

    /// Starts a session with a paired host: the client's window opens.
    /// A running session ends first.
    pub fn connect(&self, id: &str) -> anyhow::Result<SessionState> {
        let trusted = self.trusted()?;
        let peer = trusted
            .peers
            .iter()
            .find(|p| p.key.fingerprint() == id)
            .with_context(|| format!("not paired with {id}"))?;
        let target = peer.address.clone().unwrap_or_else(|| peer.name.clone());
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut old) = session.take() {
            old.stop();
        }
        let mut child = Command::new(&self.client)
            .arg(&target)
            .arg("--app")
            .arg("--state-dir")
            .arg(&self.dir)
            .args(&self.client_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", self.client.display()))?;
        let stats = Arc::new(Mutex::new(None));
        let log = Arc::new(Mutex::new(VecDeque::new()));
        let stdout = child.stdout.take().expect("piped");
        let into = stats.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.starts_with('{')
                    && let Ok(v) = serde_json::from_str::<Value>(&line)
                {
                    *into.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
                }
            }
        });
        let stderr = child.stderr.take().expect("piped");
        let into = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut log = into.lock().unwrap_or_else(|e| e.into_inner());
                if log.len() == LOG_LINES {
                    log.pop_front();
                }
                log.push_back(line);
            }
        });
        *session = Some(Running {
            device_id: id.to_owned(),
            device_name: peer.name.clone(),
            stdin: child.stdin.take(),
            child,
            stats,
            log,
        });
        drop(session);
        Ok(self.session())
    }

    /// The running (or last) session.
    pub fn session(&self) -> SessionState {
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = session.as_mut() else {
            return SessionState::default();
        };
        let exited = r.child.try_wait().ok().flatten();
        SessionState {
            active: exited.is_none(),
            device_id: Some(r.device_id.clone()),
            device_name: Some(r.device_name.clone()),
            stats: r.stats.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            error: exited.filter(|s| !s.success()).map(|_| r.error()),
        }
    }

    /// Ends the session (the client's window closes).
    pub fn disconnect(&self) {
        let running = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        drop(running);
    }

    fn control_path(&self) -> PathBuf {
        self.control
            .clone()
            .unwrap_or_else(control::running_host_path)
    }

    /// This computer, and its host if one runs.
    pub fn this_machine(&self) -> ThisMachine {
        ThisMachine {
            name: self.name.clone(),
            host: control::request(&self.control_path(), &json!({"cmd": "status"})).ok(),
        }
    }

    /// Opens pairing on this computer's host: the PIN for the new device.
    pub fn open_pairing(&self) -> anyhow::Result<Value> {
        control::request(&self.control_path(), &json!({"cmd": "pair"}))
    }

    /// Removes a device paired with this computer's host.
    pub fn unpair_from_host(&self, device: &str) -> anyhow::Result<()> {
        control::request(
            &self.control_path(),
            &json!({"cmd": "unpair", "device": device}),
        )
        .map(drop)
    }
}

/// The device list from the paired hosts and the answers.
pub fn merge(trusted: &Trusted, found: &[FoundHost]) -> Vec<Device> {
    let mut devices: Vec<Device> = trusted
        .peers
        .iter()
        .map(|p| {
            let f = found.iter().find(|f| f.key == p.key);
            Device {
                id: p.key.fingerprint(),
                name: p.name.clone(),
                online: f.is_some(),
                favorite: false,
                os: f.map(|f| f.os.clone()).unwrap_or_default(),
                gpu: f.map(|f| f.gpu.clone()).unwrap_or_default(),
                paired: true,
                pairing: f.is_some_and(|f| f.pairing),
                busy: f.is_some_and(|f| f.busy),
                address: f.map(|f| f.addr.to_string()).or_else(|| p.address.clone()),
            }
        })
        .collect();
    for f in found {
        if trusted.get(&f.key).is_none() {
            devices.push(Device {
                id: f.key.fingerprint(),
                name: f.name.clone(),
                online: true,
                favorite: false,
                os: f.os.clone(),
                gpu: f.gpu.clone(),
                paired: false,
                pairing: f.pairing,
                busy: f.busy,
                address: Some(f.addr.to_string()),
            });
        }
    }
    devices.sort_by(|a, b| {
        (!a.paired, !a.online, &a.name, &a.id).cmp(&(!b.paired, !b.online, &b.name, &b.id))
    });
    devices
}

/// The default state directory (as the command-line client's).
pub fn default_dir() -> PathBuf {
    fernsicht_client::default_state_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fernsicht_secure::{Peer, PublicKey};

    fn found(name: &str, key: u8, addr: &str) -> FoundHost {
        FoundHost {
            addr: addr.parse().unwrap(),
            name: name.into(),
            key: PublicKey([key; 32]),
            pairing: key == 3,
            busy: key == 1,
            os: "Bazzite".into(),
            gpu: "Radeon · H.264".into(),
        }
    }

    fn peer(name: &str, key: u8, addr: &str) -> Peer {
        Peer {
            name: name.into(),
            key: PublicKey([key; 32]),
            address: Some(addr.into()),
            paired_at: 0,
        }
    }

    #[test]
    fn the_list_merges_paired_and_found() {
        let mut t = Trusted::default();
        t.add(peer("zentrale", 1, "192.168.178.87:47800"));
        t.add(peer("buero", 2, "192.168.178.50:47800"));
        let list = merge(
            &t,
            &[
                found("zentrale", 1, "192.168.178.87:47800"),
                found("neu", 3, "192.168.178.66:47800"),
            ],
        );
        let names: Vec<&str> = list.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["zentrale", "buero", "neu"], "paired, online first");
        assert!(list[0].online && list[0].paired && list[0].busy);
        assert_eq!(list[0].gpu, "Radeon · H.264");
        assert!(!list[1].online && list[1].paired);
        assert_eq!(list[1].address.as_deref(), Some("192.168.178.50:47800"));
        assert!(list[2].online && !list[2].paired && list[2].pairing);
        assert_eq!(list[2].id, PublicKey([3; 32]).fingerprint());
    }

    #[test]
    fn the_client_is_found() {
        // SAFETY: tests in this binary do not read this variable concurrently.
        unsafe { std::env::set_var("FERNSICHT_CLIENT", "/opt/x/fernsicht-client") };
        assert_eq!(find_client(), PathBuf::from("/opt/x/fernsicht-client"));
        unsafe { std::env::remove_var("FERNSICHT_CLIENT") };
        let c = find_client();
        assert!(c.ends_with("fernsicht-client"), "{c:?}");
        assert!(!hostname().is_empty());
    }
}
