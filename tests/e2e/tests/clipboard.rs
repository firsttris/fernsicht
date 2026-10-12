//! The clipboard end to end: a real host and client, each on a desktop of
//! its own (two compositors with data control). Copied on the client
//! before the session, the text arrives on the host when it starts (the
//! client's clipboard wins); copied on the host, it arrives on the client.
//! Runs where `FERNSICHT_WAYLAND_SOCKET` and `FERNSICHT_WAYLAND_SOCKET_2`
//! name two such compositors (headless Sway in CI, `kwin_wayland
//! --virtual` by hand), else it skips.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use fernsicht_client::{ClientConfig, InputHandle};
use fernsicht_clipboard::{Clipboard, Current, Place};
use fernsicht_e2e::Host;
use fernsicht_host_agent::{HostConfig, InputKind};
use fernsicht_input::Recorder;

fn sockets() -> Option<(PathBuf, PathBuf)> {
    let var = |n| std::env::var_os(n).map(PathBuf::from);
    let both = var("FERNSICHT_WAYLAND_SOCKET").zip(var("FERNSICHT_WAYLAND_SOCKET_2"));
    if both.is_none() {
        eprintln!("skipped: set FERNSICHT_WAYLAND_SOCKET and _2 (two compositors)");
    }
    both
}

/// The next text `c` sees within `secs` seconds that `want` accepts.
fn wait_for(c: &Clipboard, secs: u64, want: &str) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match c.changes().recv_timeout(left) {
            Ok(t) if t == want => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

#[test]
fn text_copied_on_either_side_arrives_on_the_other() {
    let Some((host_desktop, client_desktop)) = sockets() else {
        return;
    };
    let _serial = fernsicht_e2e::exclusive();
    // What a user does on each desktop.
    let on_host = Clipboard::at(&host_desktop, Current::Skip).unwrap();
    let on_client = Clipboard::at(&client_desktop, Current::Skip).unwrap();
    on_client.set("vor der Sitzung kopiert".into());
    std::thread::sleep(Duration::from_millis(200));

    let host = Host::start(HostConfig {
        input: InputKind::Record(Recorder::default()),
        clipboard: Place::At(host_desktop.clone()),
        ..HostConfig::default()
    });
    let cfg = ClientConfig {
        host: host.addr().to_string(),
        duration: Some(Duration::from_secs(4)),
        input: Some(Arc::new(InputHandle::default())),
        clipboard: Place::At(client_desktop.clone()),
        ..ClientConfig::default()
    };
    let client =
        std::thread::spawn(move || fernsicht_client::run(cfg, Arc::new(AtomicBool::new(false))));
    assert!(
        wait_for(&on_host, 3, "vor der Sitzung kopiert"),
        "the client's clipboard did not arrive on the host"
    );
    on_host.set("auf dem Host kopiert: ä ö ü".into());
    assert!(
        wait_for(&on_client, 3, "auf dem Host kopiert: ä ö ü"),
        "the host's clipboard did not arrive on the client"
    );
    client.join().unwrap().unwrap();
    host.stop();
}
