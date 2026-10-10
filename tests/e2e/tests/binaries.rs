//! The shipped binaries: CLI, a real host/client run over localhost.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn bin(name: &str) -> Command {
    static HOST: OnceLock<std::path::PathBuf> = OnceLock::new();
    static CLIENT: OnceLock<std::path::PathBuf> = OnceLock::new();
    let (cell, package) = match name {
        "fernsicht-host-agent" => (&HOST, "fernsicht-host-agent"),
        "fernsicht-client" => (&CLIENT, "fernsicht-client"),
        other => panic!("unknown binary {other}"),
    };
    let path = cell.get_or_init(|| {
        escargot::CargoBuild::new()
            .package(package)
            .bin(name)
            .run()
            .unwrap_or_else(|e| panic!("building {name}: {e}"))
            .path()
            .to_path_buf()
    });
    Command::new(path)
}

/// The host binary, killed when dropped (also when a test panics).
struct HostProcess {
    child: Child,
    addr: String,
    /// The pairing PIN it showed (with `--pair`).
    pin: String,
    /// Its control socket.
    control: std::path::PathBuf,
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A fresh directory for keys and paired devices.
fn state_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("fernsicht-bin-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// The PIN in "Kopplung offen für 5 Minuten. PIN: 123456".
fn pin_in(line: &str) -> String {
    line.split("PIN: ")
        .nth(1)
        .unwrap_or_else(|| panic!("no PIN in {line:?}"))
        .trim()
        .to_string()
}

/// Starts the host binary on an ephemeral port, in pairing mode.
fn spawn_host(dir: &std::path::Path) -> HostProcess {
    spawn_host_with(dir, true)
}

fn spawn_host_with(dir: &std::path::Path, pair: bool) -> HostProcess {
    let control = dir.join("control.sock");
    let mut child = bin("fernsicht-host-agent")
        .args(["--bind", "127.0.0.1:0", "--state-dir"])
        .arg(dir)
        .arg("--control")
        .arg(&control)
        .args(pair.then_some("--pair"))
        .env("RUST_LOG", "info")
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut out_lines = BufReader::new(stdout).lines();
    let pin = if pair {
        // The PIN comes first on stdout.
        pin_in(&out_lines.next().expect("host exited").unwrap())
    } else {
        String::new()
    };
    std::thread::spawn(move || out_lines.for_each(drop));
    let mut host = HostProcess {
        child,
        addr: String::new(),
        pin,
        control,
    };
    let stderr = host.child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let line = lines.next().expect("host exited").unwrap();
        if let Some(addr) = line.split("listening on ").nth(1) {
            host.addr = addr.trim().to_string();
        }
        // The control socket is up after the address.
        if line.contains("control socket") {
            // Keep draining stderr so the host never blocks on a full pipe.
            std::thread::spawn(move || lines.for_each(drop));
            return host;
        }
    }
    panic!("host did not start");
}

/// Runs a command against the host's control socket; its stdout.
fn host_command(host: &HostProcess, args: &[&str]) -> String {
    let out = bin("fernsicht-host-agent")
        .args(args)
        .arg("--control")
        .arg(&host.control)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{args:?}: {stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

#[test]
fn a_running_host_pairs_through_its_control_socket() {
    let host = spawn_host_with(&state_dir("control-host"), false);
    let client_dir = state_dir("control-client");
    let status = host_command(&host, &["status"]);
    assert!(status.contains("Gekoppelte Geräte: 0"), "{status}");
    assert!(status.contains("Keine Verbindung"), "{status}");

    let pin = pin_in(host_command(&host, &["pair"]).lines().next().unwrap());
    assert!(host_command(&host, &["status"]).contains("Kopplung offen"));
    let out = bin("fernsicht-client")
        .args(["pair", &host.addr, &pin, "--name", "sofa", "--state-dir"])
        .arg(&client_dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status = host_command(&host, &["status"]);
    assert!(status.contains("Gekoppelte Geräte: 1"), "{status}");
    assert!(status.contains("sofa"), "{status}");
    assert!(!status.contains("Kopplung offen"), "closed after pairing");

    host_command(&host, &["unpair", "sofa"]);
    assert!(host_command(&host, &["status"]).contains("Gekoppelte Geräte: 0"));
    // The forgotten device is turned away.
    let out = bin("fernsicht-client")
        .args([&host.addr, "--duration", "1", "--no-audio", "--state-dir"])
        .arg(&client_dir)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not paired"), "{err}");

    // No host there: a plain error, not a hang.
    let out = bin("fernsicht-host-agent")
        .args(["status", "--control"])
        .arg(state_dir("nobody").join("control.sock"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is the host running"));
}

#[test]
fn help_and_version() {
    for name in ["fernsicht-host-agent", "fernsicht-client"] {
        let out = bin(name).arg("--help").output().unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("Usage"));
        let out = bin(name).arg("--version").output().unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")));
    }
}

#[test]
fn bad_arguments_exit_with_usage_error() {
    let out = bin("fernsicht-client").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "host argument is required");
    let out = bin("fernsicht-client")
        .args(["127.0.0.1:1", "--fps", "fast"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = bin("fernsicht-host-agent")
        .args(["--loss", "lots"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn host_reports_bind_errors() {
    let taken = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let out = bin("fernsicht-host-agent")
        .args(["--bind", &addr, "--state-dir"])
        .arg(state_dir("bind"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(&addr));
}

#[test]
fn host_and_client_binaries_pair_and_stream() {
    let host = spawn_host(&state_dir("host"));
    let client_dir = state_dir("client");
    // Pair with the PIN the host showed.
    let out = bin("fernsicht-client")
        .args([
            "pair",
            &host.addr,
            &host.pin,
            "--name",
            "test-client",
            "--state-dir",
        ])
        .arg(&client_dir)
        .output()
        .unwrap();
    let paired = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{paired}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(paired.contains("Gekoppelt mit"), "{paired}");
    let out = bin("fernsicht-client")
        .args(["hosts", "--state-dir"])
        .arg(&client_dir)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains(&host.addr));
    // Found in the network (asked directly here: no broadcasts on
    // loopback), and known as paired.
    let out = bin("fernsicht-client")
        .args(["discover", &host.addr, "--wait", "0.5", "--state-dir"])
        .arg(&client_dir)
        .output()
        .unwrap();
    let found = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{found}");
    let line = found
        .lines()
        .find(|l| l.contains(&host.addr))
        .unwrap_or_else(|| panic!("host not found: {found}"));
    assert!(
        line.contains("gekoppelt") && line.contains("Testbild"),
        "{line}"
    );
    // Connect by address (the name works the same).
    let out = bin("fernsicht-client")
        .args([
            &host.addr,
            "--width",
            "640",
            "--height",
            "360",
            "--duration",
            "3",
            "--no-audio",
            "--state-dir",
        ])
        .arg(&client_dir)
        .args(["--loss", "0.01"])
        .output()
        .unwrap();
    drop(host);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Overlay lines with the stage names from the UI.
    for needle in [
        "Glass-to-Glass",
        "Capture",
        "Encode",
        "Netz",
        "Decode",
        "Anzeige",
        "Verlust (FEC)",
        "Zusammenfassung",
    ] {
        assert!(stdout.contains(needle), "missing {needle:?} in:\n{stdout}");
    }
    let frames = stdout
        .lines()
        .find(|l| l.starts_with("Frames:"))
        .expect("summary line");
    // "Frames: N angezeigt, M komplett, L verloren · …"
    let number_before = |word: &str| -> u32 {
        let head = &frames[..frames
            .find(word)
            .unwrap_or_else(|| panic!("{word}: {frames}"))];
        head.split(|c: char| !c.is_ascii_digit())
            .rfind(|t| !t.is_empty())
            .unwrap()
            .parse()
            .unwrap()
    };
    let (shown, complete, lost) = (
        number_before(" angezeigt"),
        number_before(" komplett"),
        number_before(" verloren"),
    );
    assert!(shown >= 120, "{frames}");
    // FEC hiding 1 % loss exactly is asserted by the scenarios, which can
    // tell network loss from the host shedding a frame under CPU load. The
    // binaries only see the total, so allow a stray frame (≤ 1 %).
    assert!(lost * 100 <= complete, "{frames}");
}

#[test]
fn an_unpaired_host_is_refused_with_a_hint() {
    let dir = state_dir("unpaired");
    let out = bin("fernsicht-client")
        .args(["192.0.2.1", "--duration", "1", "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not paired with 192.0.2.1"), "{err}");
    assert!(err.contains("fernsicht-client pair"), "{err}");
    // A wrong PIN is said plainly.
    let host = spawn_host(&state_dir("host-wrong-pin"));
    let wrong = if host.pin == "000000" {
        "000001"
    } else {
        "000000"
    };
    let out = bin("fernsicht-client")
        .args(["pair", &host.addr, wrong, "--state-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("wrong PIN"));
}

#[test]
fn for_the_app_the_client_prints_json_and_stops_when_stdin_closes() {
    let host = spawn_host(&state_dir("app-host"));
    let client_dir = state_dir("app-client");
    let out = bin("fernsicht-client")
        .args(["pair", &host.addr, &host.pin, "--state-dir"])
        .arg(&client_dir)
        .output()
        .unwrap();
    assert!(out.status.success());
    let mut child = bin("fernsicht-client")
        .args([
            &host.addr,
            "--app",
            "--headless",
            "--no-audio",
            "--width",
            "640",
            "--height",
            "360",
            "--duration",
            "60",
            "--state-dir",
        ])
        .arg(&client_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let lines = std::thread::spawn(move || {
        BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .collect::<Vec<_>>()
    });
    std::thread::sleep(Duration::from_millis(2500));
    let started = Instant::now();
    drop(child.stdin.take());
    let status = child.wait().unwrap();
    assert!(status.success());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "stopped when stdin closed, not after 60 s"
    );
    let stats: Vec<String> = lines
        .join()
        .unwrap()
        .into_iter()
        .filter(|l| l.starts_with('{'))
        .collect();
    assert!(!stats.is_empty(), "one JSON line per second");
    for l in &stats {
        assert!(
            l.contains("\"glassToGlassUs\"") && l.contains("\"stagesUs\""),
            "{l}"
        );
    }
}
