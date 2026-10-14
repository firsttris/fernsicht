//! Connecting over the internet (`docs/internet.md`), with the real host,
//! client and rendezvous service on this machine: the client reaches the
//! host only through the service, a STUN server here tells each side its
//! address, and the session streams on the path that answered. With
//! coturn installed (`turnserver`), also through a TURN relay.
//!
//! Real NATs (network namespaces) are in `tests/nat/run.sh`.

use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fernsicht_client::internet::{Options, over_internet};
use fernsicht_client::{ClientConfig, ClientSecurity, Identity, Peer, Trusted, run};
use fernsicht_e2e::{Host, exclusive};
use fernsicht_host_agent::{HostConfig, HostSecurity, HostSettings};
use fernsicht_internet::candidates::Rung;
use fernsicht_internet::stun::{self, Builder, Class, Message, attr, method};

/// A STUN server answering Binding requests with the address it saw.
fn stun_server(stop: Arc<AtomicBool>) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let addr = socket.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1500];
        while !stop.load(Ordering::Relaxed) {
            let Ok((n, from)) = socket.recv_from(&mut buf) else {
                continue;
            };
            let Ok(m) = Message::parse(&buf[..n]) else {
                continue;
            };
            if (m.method, m.class) == (method::BINDING, Class::Request) {
                let answer = Builder::new(method::BINDING, Class::Success, m.transaction)
                    .xor_address(attr::XOR_MAPPED_ADDRESS, from)
                    .fingerprint()
                    .finish();
                let _ = socket.send_to(&answer, from);
            }
        }
    });
    addr
}

/// The rendezvous service, the STUN server, a host registered there and a
/// client paired with it.
struct Internet {
    url: String,
    stun: SocketAddr,
    stop: Arc<AtomicBool>,
    /// Runs while the test does.
    _host: Host,
    host_key: fernsicht_client::PublicKey,
    client: Identity,
}

impl Drop for Internet {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn internet(turn: Option<fernsicht_rendezvous::turn::TurnConfig>) -> Internet {
    let stop = Arc::new(AtomicBool::new(false));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    fernsicht_rendezvous::serve(
        listener,
        fernsicht_rendezvous::Config { turn },
        stop.clone(),
    )
    .unwrap();
    let stun = stun_server(stop.clone());
    let client = Identity::generate();
    let mut paired = Trusted::default();
    paired.add(Peer {
        name: "laptop".into(),
        key: client.public,
        address: None,
        paired_at: 0,
        rendezvous: None,
    });
    let sec = Arc::new(HostSecurity::new(
        Identity::generate(),
        "zentrale",
        paired,
        None,
    ));
    let host_key = sec.public_key();
    let host = Host::start(HostConfig {
        security: Some(sec),
        settings: Arc::new(HostSettings::default().with_rendezvous(Some(url.clone()))),
        stun: vec![stun.to_string()],
        ..HostConfig::default()
    });
    Internet {
        url,
        stun,
        stop,
        _host: host,
        host_key,
        client,
    }
}

/// Connects through the service, then streams for a moment on what it
/// found.
fn connect_and_stream(net: &Internet, relay_only: bool) -> (Rung, fernsicht_client::RunSummary) {
    // The host registers in the background.
    let deadline = Instant::now() + Duration::from_secs(10);
    let reached = loop {
        let options = Options {
            stun: vec![net.stun.to_string()],
            relay_only,
        };
        match over_internet(&net.url, &net.host_key, &net.client, &options) {
            Ok(r) => break r,
            Err(e) if Instant::now() < deadline => {
                eprintln!("not yet: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => panic!("{e}"),
        }
    };
    let path = reached.path;
    let cfg = ClientConfig {
        host: reached.host.to_string(),
        socket: Some(Arc::new(reached.socket)),
        width: 640,
        height: 360,
        duration: Some(Duration::from_secs(2)),
        host_timeout: Duration::from_secs(3),
        security: Some(Arc::new(ClientSecurity {
            identity: net.client.clone(),
            host: net.host_key,
        })),
        ..ClientConfig::default()
    };
    let summary = run(cfg, Arc::new(AtomicBool::new(false))).unwrap();
    drop(reached.relay);
    (path, summary)
}

#[test]
fn a_client_reaches_its_host_through_the_rendezvous_service() {
    let _serial = exclusive();
    let net = internet(None);
    let (path, s) = connect_and_stream(&net, false);
    // The host listens on 127.0.0.1 only: what the STUN server saw is
    // the address that answered.
    assert!(path.is_direct(), "{path:?}");
    assert!(s.frames_presented > 60, "{s:?}");
    assert!(s.rtt_us.is_some(), "sealed pings over the punched path");
    assert_eq!(
        s.rendezvous.as_deref(),
        Some(net.url.as_str()),
        "the host named its service"
    );
}

#[test]
fn an_unpaired_device_is_refused_through_the_service() {
    let _serial = exclusive();
    let net = internet(None);
    let stranger = Identity::generate();
    let options = Options {
        stun: vec![net.stun.to_string()],
        relay_only: false,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let failure = loop {
        match over_internet(&net.url, &net.host_key, &stranger, &options) {
            Ok(_) => panic!("a stranger got a path"),
            Err(f) if f.code == fernsicht_client::FailureCode::NotPaired => break f,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            Err(f) => panic!("{f}"),
        }
    };
    assert!(failure.message.contains("not paired"), "{failure}");
    // And a host nobody registered is not there.
    let nobody = Identity::generate().public;
    let e = over_internet(&net.url, &nobody, &net.client, &options)
        .err()
        .unwrap();
    assert_eq!(e.code, fernsicht_client::FailureCode::Unreachable, "{e}");
}

/// coturn on this machine, if installed: `(the process, its port)`.
fn coturn(secret: &str) -> Option<(Child, u16)> {
    let port = {
        let s = UdpSocket::bind("127.0.0.1:0").ok()?;
        s.local_addr().ok()?.port()
    };
    let child = Command::new("turnserver")
        .args([
            "--listening-ip=127.0.0.1",
            "--relay-ip=127.0.0.1",
            &format!("--listening-port={port}"),
            "--min-port=49152",
            "--max-port=65535",
            "--use-auth-secret",
            &format!("--static-auth-secret={secret}"),
            "--realm=fernsicht",
            "--allow-loopback-peers",
            "--no-tls",
            "--no-dtls",
            "--no-cli",
            "--no-tcp-relay",
            "-n",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Ready when it answers a Binding request.
    let probe = UdpSocket::bind("127.0.0.1:0").ok()?;
    probe
        .set_read_timeout(Some(Duration::from_millis(100)))
        .ok()?;
    let mut buf = [0u8; 512];
    for _ in 0..50 {
        let _ = probe.send_to(&stun::binding_request([9; 12]), ("127.0.0.1", port));
        if probe.recv(&mut buf).is_ok() {
            return Some((child, port));
        }
    }
    None
}

#[test]
fn a_client_reaches_its_host_through_a_turn_relay() {
    let _serial = exclusive();
    let Some((mut turnserver, port)) = coturn("north") else {
        // CI installs it: there a missing coturn is a failure, not a skip.
        assert!(
            std::env::var_os("FERNSICHT_REQUIRE_TURN").is_none(),
            "FERNSICHT_REQUIRE_TURN is set, but coturn (turnserver) does not start"
        );
        eprintln!("coturn (turnserver) not installed: the relay is not checked");
        return;
    };
    let turn = fernsicht_rendezvous::turn::TurnConfig {
        urls: vec![format!("turn:127.0.0.1:{port}?transport=udp")],
        secret: "north".into(),
        ttl: Duration::from_secs(600),
    };
    let net = internet(Some(turn));
    let (path, s) = connect_and_stream(&net, true);
    let _ = turnserver.kill();
    assert_eq!(path, Rung::Relayed);
    assert!(s.frames_presented > 60, "{s:?}");
    assert!(s.rtt_us.is_some());
}
