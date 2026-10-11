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

/// Sends forged packets "from the host" every `every` until `stop`.
fn forging(
    link: &Arc<ImpairedLink>,
    every: Duration,
    stop: &Arc<AtomicBool>,
    packets: Vec<Vec<u8>>,
) -> std::thread::JoinHandle<()> {
    let (link, stop) = (link.clone(), stop.clone());
    std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            for p in &packets {
                link.forge_to_client(p);
            }
            std::thread::sleep(every);
        }
    })
}

#[test]
fn forged_packets_neither_stop_nor_end_a_secure_session() {
    use fernsicht_proto::{Bye, Handshake, RejectReason};
    let _serial = exclusive();
    let sec = host_security();
    sec.open_pairing("112233");
    let host = Host::start(HostConfig {
        security: Some(sec.clone()),
        ..HostConfig::default()
    });
    let link = Arc::new(ImpairedLink::start(
        host.addr(),
        Impairment::none(),
        Impairment::none(),
        3,
    ));
    let me = Identity::generate();
    let peer = pair(&link.addr().to_string(), "112233", &me, "laptop").unwrap();
    // From the start: forged handshake answers, rejections and goodbyes
    // with the host's address, as someone on the LAN could send them.
    let mut buf = [0u8; 256];
    let packet = |n: usize, buf: &[u8; 256]| buf[..n].to_vec();
    let n = Handshake {
        reply: true,
        message: &[7u8; 96],
    }
    .encode(&mut buf);
    let forged_answer = packet(n, &buf);
    let n = RejectReason::NotPaired.encode(&mut buf);
    let reject = packet(n, &buf);
    let n = Bye { session_id: 0 }.encode(&mut buf);
    let bye = packet(n, &buf);
    let stop = Arc::new(AtomicBool::new(false));
    // The link learns the client's address from its first packet.
    let forger = {
        let link = link.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                link.forge_to_client(&forged_answer);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let rejecter = forging(&link, Duration::from_millis(50), &stop, vec![reject, bye]);
    let s = run(
        client_cfg(
            link.addr().to_string(),
            Some(Arc::new(ClientSecurity {
                identity: me,
                host: peer.key,
            })),
            2.5,
        ),
        Arc::new(AtomicBool::new(false)),
    );
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    forger.join().unwrap();
    rejecter.join().unwrap();
    let s = s.expect("the forged packets ended the session");
    assert!(s.frames_presented > 100, "{s:?}");
    drop(link);
    host.stop();
}

#[test]
fn garbage_does_not_keep_a_silent_host_alive() {
    let _serial = exclusive();
    let sec = host_security();
    sec.open_pairing("445566");
    let host = Host::start(HostConfig {
        security: Some(sec.clone()),
        ..HostConfig::default()
    });
    // The host's packets stop after a second; garbage keeps coming.
    let down = Impairment {
        blackout: Some((Duration::from_secs(1), Duration::from_secs(30))),
        ..Impairment::none()
    };
    let link = Arc::new(ImpairedLink::start(
        host.addr(),
        down,
        Impairment::none(),
        5,
    ));
    let me = Identity::generate();
    let peer = {
        // Pairing straight to the host (the blackout would stop it).
        pair(&host.addr().to_string(), "445566", &me, "laptop").unwrap()
    };
    let stop = Arc::new(AtomicBool::new(false));
    let noise = forging(
        &link,
        Duration::from_millis(50),
        &stop,
        vec![vec![0xf5, 1, 99, 0, 1, 2, 3]],
    );
    let started = std::time::Instant::now();
    let r = run(
        client_cfg(
            link.addr().to_string(),
            Some(Arc::new(ClientSecurity {
                identity: me,
                host: peer.key,
            })),
            10.0,
        ),
        Arc::new(AtomicBool::new(false)),
    );
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    noise.join().unwrap();
    let e = r.expect_err("garbage kept the session alive");
    assert!(e.to_string().contains("no packets from"), "{e:#}");
    assert!(started.elapsed() < Duration::from_secs(6));
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
