//! The app's logic against a real host (in this process) and the real
//! client program (headless): find, pair, connect, overlay, disconnect,
//! forget, and the host on "this machine".

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fernsicht_desktop::backend::Backend;
use fernsicht_host_agent::{HostAgent, HostConfig, HostDescription, HostSecurity};
use fernsicht_secure::{Identity, Trusted};

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fernsicht-app-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn client_program() -> PathBuf {
    escargot::CargoBuild::new()
        .manifest_path(concat!(env!("CARGO_MANIFEST_DIR"), "/../client/Cargo.toml"))
        .bin("fernsicht-client")
        .run()
        .expect("building fernsicht-client")
        .path()
        .to_path_buf()
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn find_pair_connect_and_forget() {
    let dir = temp("state");
    let control = dir.join("host-control.sock");
    let sec = Arc::new(HostSecurity::new(
        Identity::generate(),
        "zentrale",
        Trusted::default(),
        None,
    ));
    let agent = HostAgent::bind(HostConfig {
        bind: "127.0.0.1:0".into(),
        security: Some(sec.clone()),
        control: Some(control.clone()),
        description: HostDescription {
            os: "Bazzite".into(),
            gpu: "Testbild".into(),
        },
        ..HostConfig::default()
    })
    .unwrap();
    let addr = agent.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let host = {
        let stop = stop.clone();
        std::thread::spawn(move || agent.run(stop))
    };
    wait_for("the control socket", || control.exists());

    let mut app = Backend::new(dir.join("client"));
    app.client = client_program();
    app.client_args = [
        "--headless",
        "--no-audio",
        "--width",
        "640",
        "--height",
        "360",
    ]
    .map(String::from)
    .to_vec();
    app.control = Some(control);
    app.extra_targets = vec![addr];
    app.discovery_wait = Duration::from_millis(300);

    // Found, not paired yet.
    let list = app.devices().unwrap();
    let zentrale = list.iter().find(|d| d.name == "zentrale").expect("found");
    assert!(zentrale.online && !zentrale.paired && !zentrale.pairing);
    assert_eq!(zentrale.os, "Bazzite");
    let id = zentrale.id.clone();
    assert_eq!(id, sec.public_key().fingerprint());

    // "This machine" runs the host: the app opens pairing there.
    let me = app.this_machine();
    let status = me.host.expect("host status");
    assert_eq!(status["name"], "zentrale");
    let pin = app.open_pairing().unwrap()["pin"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        app.devices()
            .unwrap()
            .iter()
            .any(|d| d.id == id && d.pairing)
    );
    let paired = app.pair(&addr.to_string(), &pin).unwrap();
    assert_eq!((paired.id.as_str(), paired.paired), (id.as_str(), true));
    assert_eq!(sec.paired().len(), 1);
    let list = app.devices().unwrap();
    assert!(list[0].id == id && list[0].paired && list[0].online);

    // A session: the overlay comes in, the host shows it, then it ends.
    assert!(app.command("mute").is_err(), "no session yet");
    let settings = fernsicht_desktop::backend::StreamSettings {
        fps: 30,
        ..Default::default()
    };
    let s = app.connect(&id, &settings).unwrap();
    assert!(s.active);
    assert_eq!(s.device_name.as_deref(), Some("zentrale"));
    wait_for("overlay stats", || app.session().stats.is_some());
    // The session's controls reach the client.
    app.command("mute").unwrap();
    app.command("gaming").unwrap();
    assert!(app.session().active, "commands do not end the session");
    let stats = app.session().stats.unwrap();
    assert!(
        stats["glassToGlassUs"]["avg"].as_u64().unwrap() > 0,
        "{stats}"
    );
    assert!(app.this_machine().host.unwrap()["session"].is_object());
    app.disconnect();
    let s = app.session();
    assert!(!s.active && s.error.is_none(), "{s:?}");

    // The host forgets this device: it is turned away, and the app says why.
    let device_name = sec.paired()[0].name.clone();
    app.unpair_from_host(&device_name).unwrap();
    app.connect(&id, &Default::default()).unwrap();
    wait_for("the client to give up", || !app.session().active);
    let err = app.session().error.unwrap();
    assert!(err.contains("not paired"), "{err}");

    // Forgotten here as well: listed as not paired.
    app.forget(&id).unwrap();
    assert!(app.forget(&id).is_err());
    assert!(
        app.devices()
            .unwrap()
            .iter()
            .any(|d| d.id == id && !d.paired)
    );
    assert!(app.connect(&id, &Default::default()).is_err(), "not paired");

    stop.store(true, Ordering::Relaxed);
    host.join().unwrap().unwrap();
    // No host here any more.
    assert!(app.this_machine().host.is_none());
    assert!(app.open_pairing().is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
