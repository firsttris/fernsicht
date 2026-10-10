//! What the app's commands do, without Tauri (so it is testable): devices
//! from LAN discovery and the paired list, pairing, sessions as child
//! processes of the native client, and the host on this machine.
//!
//! State lives where the command-line client keeps it (`~/.config/fernsicht`),
//! so pairings made in a terminal show up in the app and the other way
//! round.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use fernsicht_client::discover::{FoundHost, broadcast_targets, discover};
use fernsicht_client::{DEFAULT_PORT, Identity, Trusted, pair, refresh_addresses};
use fernsicht_host_agent::control;
use serde::{Deserialize, Serialize};
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

/// How a session should look, from the settings page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StreamSettings {
    /// Stream size; 0 = the host's screen.
    pub width: u16,
    pub height: u16,
    /// Frames per second; 0 = 60.
    pub fps: u16,
    /// Mbit/s; 0 = the host's choice for the size.
    pub bitrate_mbit: u32,
    /// Start in gaming mode (a click captures the pointer).
    pub gaming: bool,
    /// Video codec; automatic picks HEVC where both sides can.
    pub codec: CodecSetting,
}

/// The settings page's video codec choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecSetting {
    #[default]
    Auto,
    H264,
    Hevc,
    Av1,
}

impl StreamSettings {
    /// The client's arguments for these settings.
    pub fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if self.width > 0 && self.height > 0 {
            a.extend([
                "--width".into(),
                self.width.to_string(),
                "--height".into(),
                self.height.to_string(),
            ]);
        }
        if self.fps > 0 {
            a.extend(["--fps".into(), self.fps.to_string()]);
        }
        if self.bitrate_mbit > 0 {
            a.extend([
                "--bitrate".into(),
                (self.bitrate_mbit.saturating_mul(1000)).to_string(),
            ]);
        }
        match self.codec {
            CodecSetting::Auto => {}
            CodecSetting::H264 => a.extend(["--codec".into(), "h264".into()]),
            CodecSetting::Hevc => a.extend(["--codec".into(), "hevc".into()]),
            CodecSetting::Av1 => a.extend(["--codec".into(), "av1".into()]),
        }
        if self.gaming {
            a.push("--gaming".into());
        }
        a
    }
}

/// The client's command for a key combination from the "send keys" menu
/// (Linux key codes, at most 6).
pub fn keys_command(codes: &[u16]) -> anyhow::Result<String> {
    anyhow::ensure!(
        (1..=6).contains(&codes.len()),
        "1 to 6 keys, not {}",
        codes.len()
    );
    let list: Vec<String> = codes.iter().map(u16::to_string).collect();
    Ok(format!("keys {}", list.join(",")))
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
    /// The host service: installed, running, which version, and whether
    /// this app can install it.
    pub service: crate::share::HostService,
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
        let mut found = discover(&targets, self.discovery_wait).context("looking for hosts")?;
        if refresh_addresses(&mut trusted, &found) {
            self.save(&trusted)?;
        }
        // The host on this computer answers too; it is "Dieser Rechner",
        // not a device to connect to.
        let own = self.own_host_key();
        found.retain(|f| own.as_deref() != Some(f.key.fingerprint().as_str()));
        let mut devices = merge(&trusted, &found);
        devices.retain(|d| own.as_deref() != Some(d.id.as_str()));
        Ok(devices)
    }

    /// The fingerprint of the host running on this computer, if one runs.
    fn own_host_key(&self) -> Option<String> {
        let status = control::request(&self.control_path(), &json!({"cmd": "status"})).ok()?;
        status["key"].as_str().map(str::to_owned)
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
    pub fn connect(&self, id: &str, settings: &StreamSettings) -> anyhow::Result<SessionState> {
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
            .args(settings.args())
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

    /// Tells the running client something (a line on its stdin): "mute",
    /// "unmute", "gaming", "desktop", "keys 29,56,111".
    pub fn command(&self, line: &str) -> anyhow::Result<()> {
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let stdin = session
            .as_mut()
            .and_then(|r| r.stdin.as_mut())
            .context("no session")?;
        writeln!(stdin, "{line}")?;
        stdin.flush()?;
        Ok(())
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
            service: crate::share::status(),
        }
    }

    /// Opens pairing on this computer's host: the PIN for the new device.
    pub fn open_pairing(&self) -> anyhow::Result<Value> {
        control::request(&self.control_path(), &json!({"cmd": "pair"}))
    }

    /// Switches the GPU boost of this computer's host (see the host's
    /// `power` module).
    pub fn set_gpu_boost(&self, on: bool) -> anyhow::Result<()> {
        control::request(
            &self.control_path(),
            &json!({"cmd": "set", "gpu_boost": on}),
        )
        .map(drop)
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
    fn key_combinations_become_a_client_command() {
        assert_eq!(keys_command(&[29, 56, 111]).unwrap(), "keys 29,56,111");
        assert!(keys_command(&[]).is_err());
        assert!(keys_command(&[1; 7]).is_err());
    }

    #[test]
    fn settings_become_client_arguments() {
        assert!(StreamSettings::default().args().is_empty());
        let s = StreamSettings {
            width: 1920,
            height: 1080,
            fps: 120,
            bitrate_mbit: 50,
            gaming: true,
            codec: CodecSetting::Hevc,
        };
        assert_eq!(
            s.args(),
            [
                "--width",
                "1920",
                "--height",
                "1080",
                "--fps",
                "120",
                "--bitrate",
                "50000",
                "--codec",
                "hevc",
                "--gaming"
            ]
        );
        // Half a size is no size.
        let odd = StreamSettings {
            width: 1920,
            ..StreamSettings::default()
        };
        assert!(odd.args().is_empty());
        let parsed: StreamSettings = serde_json::from_str(r#"{"fps":30,"bitrateMbit":8}"#).unwrap();
        assert_eq!(parsed.args(), ["--fps", "30", "--bitrate", "8000"]);
        // Older UIs send no codec: automatic, no argument.
        let h264: StreamSettings = serde_json::from_str(r#"{"codec":"h264"}"#).unwrap();
        assert_eq!(h264.args(), ["--codec", "h264"]);
        let av1: StreamSettings = serde_json::from_str(r#"{"codec":"av1"}"#).unwrap();
        assert_eq!(av1.args(), ["--codec", "av1"]);
        assert!(serde_json::from_str::<StreamSettings>(r#"{"codec":"vp9"}"#).is_err());
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
