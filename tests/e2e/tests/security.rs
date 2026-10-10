//! Pairing and encrypted sessions between the real host agent and client.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use fernsicht_client::{
    ClientConfig, ClientSecurity, Identity, InputEvent, InputHandle, Trusted, pair, run,
};
use fernsicht_e2e::{Host, ImpairedLink, Impairment, exclusive};
use fernsicht_host_agent::{HostConfig, HostSecurity, InputKind};
use fernsicht_input::Recorder;

fn host_security() -> Arc<HostSecurity> {
    Arc::new(HostSecurity::new(
        Identity::generate(),
        "zentrale",
        Trusted::default(),
        None,
    ))
}

fn client_cfg(addr: String, security: Option<Arc<ClientSecurity>>, secs: f32) -> ClientConfig {
    ClientConfig {
        host: addr,
        width: 640,
        height: 360,
        duration: Some(Duration::from_secs_f32(secs)),
        host_timeout: Duration::from_secs(2),
        security,
        ..ClientConfig::default()
    }
}

#[test]
fn pair_then_stream_and_type_encrypted() {
    let _serial = exclusive();
    let sec = host_security();
    sec.open_pairing("246810");
    let recorder = Recorder::default();
    let host = Host::start(HostConfig {
        security: Some(sec.clone()),
        input: InputKind::Record(recorder.clone()),
        ..HostConfig::default()
    });
    let me = Identity::generate();
    let peer = pair(&host.addr().to_string(), "246810", &me, "bazzite").unwrap();
    assert_eq!(peer.key, sec.public_key());
    assert_eq!(peer.name, "zentrale");
    assert_eq!(
        peer.address.as_deref(),
        Some(host.addr().to_string().as_str())
    );
    assert_eq!(sec.paired().len(), 1);

    let input = Arc::new(InputHandle::default());
    for code in [30u16, 48] {
        input.push(InputEvent::Key {
            code,
            pressed: true,
        });
        input.push(InputEvent::Key {
            code,
            pressed: false,
        });
    }
    let mut cfg = client_cfg(
        host.addr().to_string(),
        Some(Arc::new(ClientSecurity {
            identity: me,
            host: peer.key,
        })),
        2.0,
    );
    cfg.input = Some(input);
    let s = run(cfg, Arc::new(AtomicBool::new(false))).unwrap();
    assert!(s.frames_presented > 60, "{s:?}");
    assert!(s.cursor_shapes >= 1 && s.cursor_positions > 60, "{s:?}");
    assert!(s.rtt_us.is_some(), "sealed clock pings work");
    host.stop();
    assert_eq!(recorder.0.lock().unwrap().len(), 4, "all keys arrived once");
}

#[test]
fn a_wrong_pin_and_an_unpaired_client_get_clear_errors() {
    let _serial = exclusive();
    let sec = host_security();
    let host = Host::start(HostConfig {
        security: Some(sec.clone()),
        ..HostConfig::default()
    });
    let addr = host.addr().to_string();
    let me = Identity::generate();
    let e = pair(&addr, "111111", &me, "c").unwrap_err().to_string();
    assert!(e.contains("not in pairing mode"), "{e}");
    sec.open_pairing("111111");
    let e = pair(&addr, "111112", &me, "c").unwrap_err().to_string();
    assert!(e.contains("wrong PIN"), "{e}");

    // Knows the host's key but was never paired.
    let cfg = client_cfg(
        addr,
        Some(Arc::new(ClientSecurity {
            identity: me,
            host: sec.public_key(),
        })),
        2.0,
    );
    let e = run(cfg, Arc::new(AtomicBool::new(false)))
        .unwrap_err()
        .to_string();
    assert!(e.contains("not paired"), "{e}");
    host.stop();
}

#[test]
fn an_impostor_host_cannot_take_the_session() {
    let _serial = exclusive();
    // The client expects one host key; a host with another key cannot
    // even read the handshake, so the client gets nothing and gives up.
    let host = Host::start(HostConfig {
        security: Some(host_security()),
        ..HostConfig::default()
    });
    let cfg = client_cfg(
        host.addr().to_string(),
        Some(Arc::new(ClientSecurity {
            identity: Identity::generate(),
            host: Identity::generate().public,
        })),
        3.0,
    );
    let e = run(cfg, Arc::new(AtomicBool::new(false)))
        .unwrap_err()
        .to_string();
    assert!(e.contains("no packets"), "{e}");
    host.stop();
}

#[test]
fn pairing_and_a_secure_session_survive_loss() {
    let _serial = exclusive();
    let sec = host_security();
    sec.open_pairing("135790");
    let host = Host::start(HostConfig {
        security: Some(sec.clone()),
        ..HostConfig::default()
    });
    // 30 % lost both ways: every step is resent until it gets through.
    let link = ImpairedLink::start(host.addr(), Impairment::loss(0.3), Impairment::loss(0.3), 9);
    let me = Identity::generate();
    let peer = pair(&link.addr().to_string(), "135790", &me, "laptop").unwrap();
    let s = run(
        client_cfg(
            link.addr().to_string(),
            Some(Arc::new(ClientSecurity {
                identity: me,
                host: peer.key,
            })),
            3.0,
        ),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    assert!(s.session_id.is_some() && s.frames_presented > 0, "{s:?}");
    drop(link);
    host.stop();
}

#[test]
fn hosts_are_found_and_tell_about_themselves() {
    use fernsicht_client::discover::discover;
    use fernsicht_host_agent::HostDescription;
    let _serial = exclusive();
    let sec = host_security();
    let host = Host::start(HostConfig {
        security: Some(sec.clone()),
        description: HostDescription {
            os: "Bazzite".into(),
            gpu: "Radeon RX 7700 XT / 7800 XT · H.264".into(),
        },
        ..HostConfig::default()
    });
    let look = || discover(&[host.addr()], Duration::from_millis(300)).unwrap();
    let found = look();
    assert_eq!(found.len(), 1, "{found:?}");
    let f = &found[0];
    assert_eq!(
        (f.name.as_str(), f.key, f.addr),
        ("zentrale", sec.public_key(), host.addr())
    );
    assert_eq!(f.os, "Bazzite");
    assert_eq!(f.gpu, "Radeon RX 7700 XT / 7800 XT · H.264");
    assert!(!f.pairing && !f.busy);

    // Pairing open shows; so does a connected client.
    sec.open_pairing("135790");
    assert!(look()[0].pairing);
    let me = Identity::generate();
    let peer = pair(&host.addr().to_string(), "135790", &me, "bazzite").unwrap();
    assert!(!look()[0].pairing, "closed after pairing");
    let cfg = client_cfg(
        host.addr().to_string(),
        Some(Arc::new(ClientSecurity {
            identity: me,
            host: peer.key,
        })),
        2.0,
    );
    let client = std::thread::spawn(move || run(cfg, Arc::new(AtomicBool::new(false))));
    std::thread::sleep(Duration::from_millis(700));
    assert!(look()[0].busy, "a session is running");
    assert!(client.join().unwrap().unwrap().frames_presented > 30);
    host.stop();

    // The library's insecure test host has no key and stays silent.
    let open = Host::start(HostConfig::default());
    assert!(
        discover(&[open.addr()], Duration::from_millis(200))
            .unwrap()
            .is_empty()
    );
    open.stop();
}
