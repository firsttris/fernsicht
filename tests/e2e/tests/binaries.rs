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
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts the host binary on an ephemeral port.
fn spawn_host() -> HostProcess {
    let child = bin("fernsicht-host-agent")
        .args(["--bind", "127.0.0.1:0"])
        .env("RUST_LOG", "info")
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut host = HostProcess {
        child,
        addr: String::new(),
    };
    let stderr = host.child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let line = lines.next().expect("host exited").unwrap();
        if let Some(addr) = line.split("listening on ").nth(1) {
            host.addr = addr.trim().to_string();
            // Keep draining stderr so the host never blocks on a full pipe.
            std::thread::spawn(move || lines.for_each(drop));
            return host;
        }
    }
    panic!("host did not report its address");
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
        .args(["--bind", &addr])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(&addr));
}

#[test]
fn host_and_client_binaries_stream() {
    let host = spawn_host();
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
        ])
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
fn client_without_host_ends_after_duration() {
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = silent.local_addr().unwrap().to_string();
    let out = bin("fernsicht-client")
        .args([&addr, "--duration", "1"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Frames: 0 angezeigt"));
}
