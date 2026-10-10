//! Protocol conformance: a raw UDP peer talks to a running host agent.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fernsicht_host_agent::{CaptureKind, EncoderKind, HostAgent, HostConfig, InputKind};
use fernsicht_input::Recorder;
use fernsicht_proto::*;

struct Host {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Host {
    fn start(cfg: HostConfig) -> Self {
        let agent = HostAgent::bind(HostConfig {
            bind: "127.0.0.1:0".into(),
            ..cfg
        })
        .unwrap();
        let addr = agent.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        Self {
            addr,
            stop,
            thread: Some(std::thread::spawn(move || agent.run(s))),
        }
    }

    fn default() -> Self {
        Self::start(HostConfig::default())
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap().unwrap();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Peer {
    sock: UdpSocket,
    buf: Vec<u8>,
}

impl Peer {
    fn new(host: SocketAddr) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.connect(host).unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        Self {
            sock,
            buf: vec![0; 2048],
        }
    }

    fn send(&self, f: impl FnOnce(&mut [u8]) -> usize) {
        let mut out = [0u8; MAX_DATAGRAM];
        let n = f(&mut out);
        self.sock.send(&out[..n]).unwrap();
    }

    fn hello(&self, width: u16, height: u16, fps: u16, bitrate_kbps: u32) {
        self.send(|b| {
            Hello {
                width,
                height,
                fps,
                bitrate_kbps,
            }
            .encode(b)
        });
    }

    /// Receives until `pick` returns `Some` or `timeout` passes.
    fn wait_for<T>(
        &mut self,
        timeout: Duration,
        mut pick: impl FnMut(Packet<'_>) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(n) = self.sock.recv(&mut self.buf)
                && let Ok(p) = Packet::decode(&self.buf[..n])
                && let Some(v) = pick(p)
            {
                return Some(v);
            }
        }
        None
    }

    fn ack(&mut self) -> HelloAck {
        self.wait_for(Duration::from_secs(2), |p| match p {
            Packet::HelloAck(a) => Some(a),
            _ => None,
        })
        .expect("no HelloAck")
    }

    fn next_video(&mut self, timeout: Duration) -> Option<VideoHeader> {
        self.wait_for(timeout, |p| match p {
            Packet::Video(h, _) => Some(h),
            _ => None,
        })
    }

    /// Drains what is already queued, then reports whether video still flows.
    fn video_flows(&mut self, within: Duration) -> bool {
        std::thread::sleep(Duration::from_millis(100));
        self.sock.set_nonblocking(true).unwrap();
        while self.sock.recv(&mut self.buf).is_ok() {}
        self.sock.set_nonblocking(false).unwrap();
        self.next_video(within).is_some()
    }
}

#[test]
fn hello_is_acked_with_capped_parameters() {
    let host = Host::start(HostConfig {
        max_width: 1280,
        max_height: 720,
        max_fps: 30,
        max_bitrate_kbps: 5_000,
        ..HostConfig::default()
    });
    let mut peer = Peer::new(host.addr);
    peer.hello(3841, 2160, 144, 50_000);
    let ack = peer.ack();
    assert_eq!((ack.width, ack.height, ack.fps), (1280, 720, 30));
    assert_eq!(ack.codec, Codec::Synthetic);
    host.shutdown();
}

#[test]
fn zero_resolution_means_the_screen_size() {
    // The test pattern stands in for a 1080p screen.
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(0, 0, 60, 0);
    let ack = peer.ack();
    assert_eq!((ack.width, ack.height), (1920, 1080));
    host.shutdown();

    // Too large for the host's limits: scaled down, aspect ratio kept.
    let host = Host::start(HostConfig {
        max_width: 640,
        max_height: 480,
        max_fps: 50,
        ..HostConfig::default()
    });
    let mut peer = Peer::new(host.addr);
    peer.hello(0, 0, 50, 0);
    let ack = peer.ack();
    assert_eq!((ack.width, ack.height, ack.fps), (640, 360, 50));
    host.shutdown();
}

#[test]
fn odd_resolutions_are_rounded_down_to_even() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(1279, 719, 60, 1_000);
    let ack = peer.ack();
    assert_eq!((ack.width, ack.height), (1278, 718));
    host.shutdown();
}

#[test]
fn video_starts_with_a_keyframe_and_carries_session_and_timestamps() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(640, 360, 60, 2_000);
    let ack = peer.ack();
    let first = peer.next_video(Duration::from_secs(2)).expect("no video");
    assert_eq!(first.session_id, ack.session_id);
    assert_eq!(first.frame_id, 0);
    assert!(first.keyframe);
    assert!(first.encoded_delta_us >= first.capture_ready_delta_us);
    let later = peer
        .wait_for(Duration::from_secs(2), |p| match p {
            Packet::Video(h, _) if h.frame_id >= 3 => Some(h),
            _ => None,
        })
        .expect("frame ids do not advance");
    assert!(!later.keyframe);
    host.shutdown();
}

#[test]
fn repeated_hello_keeps_the_session() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(640, 360, 60, 2_000);
    let a = peer.ack();
    peer.hello(640, 360, 60, 2_000);
    let b = peer.ack();
    assert_eq!(a.session_id, b.session_id);
    host.shutdown();
}

#[test]
fn clock_ping_is_echoed() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.send(|b| {
        ClockPing {
            seq: 77,
            client_send_us: 123_456,
        }
        .encode(b)
    });
    let pong = peer
        .wait_for(Duration::from_secs(2), |p| match p {
            Packet::ClockPong(p) => Some(p),
            _ => None,
        })
        .expect("no pong");
    assert_eq!((pong.seq, pong.client_send_us), (77, 123_456));
    assert!(pong.host_send_us >= pong.host_recv_us);
    host.shutdown();
}

#[test]
fn garbage_is_ignored() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    let junk: [&[u8]; 5] = [
        b"",
        b"\x00",
        b"hello world",
        &[MAGIC, 99, 1, 0],
        &[MAGIC, VERSION, 200, 0],
    ];
    for j in junk {
        peer.sock.send(j).unwrap();
    }
    peer.hello(320, 240, 30, 500);
    peer.ack();
    host.shutdown();
}

#[test]
fn bye_stops_the_stream() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(320, 240, 60, 1_000);
    let ack = peer.ack();
    assert!(peer.video_flows(Duration::from_secs(1)));
    peer.send(|b| {
        Bye {
            session_id: ack.session_id,
        }
        .encode(b)
    });
    assert!(!peer.video_flows(Duration::from_millis(400)));
    host.shutdown();
}

#[test]
fn bye_with_wrong_session_is_ignored() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(320, 240, 60, 1_000);
    let ack = peer.ack();
    peer.send(|b| {
        Bye {
            session_id: ack.session_id ^ 1,
        }
        .encode(b)
    });
    assert!(peer.video_flows(Duration::from_secs(1)));
    host.shutdown();
}

#[test]
fn new_peer_replaces_the_session() {
    let host = Host::default();
    let mut a = Peer::new(host.addr);
    a.hello(320, 240, 60, 1_000);
    let ack_a = a.ack();
    let mut b = Peer::new(host.addr);
    b.hello(320, 240, 60, 1_000);
    let ack_b = b.ack();
    assert_ne!(ack_a.session_id, ack_b.session_id);
    assert!(b.video_flows(Duration::from_secs(1)));
    assert!(!a.video_flows(Duration::from_millis(400)));
    host.shutdown();
}

#[test]
fn feedback_from_a_stranger_is_ignored() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(320, 240, 60, 1_000);
    let ack = peer.ack();
    let stranger = Peer::new(host.addr);
    stranger.send(|b| {
        Bye {
            session_id: ack.session_id,
        }
        .encode(b)
    });
    assert!(peer.video_flows(Duration::from_secs(1)));
    host.shutdown();
}

#[test]
fn silent_client_times_out() {
    let host = Host::start(HostConfig {
        client_timeout: Duration::from_millis(300),
        ..HostConfig::default()
    });
    let mut peer = Peer::new(host.addr);
    peer.hello(320, 240, 60, 1_000);
    peer.ack();
    assert!(peer.next_video(Duration::from_millis(500)).is_some());
    std::thread::sleep(Duration::from_millis(500));
    assert!(!peer.video_flows(Duration::from_millis(400)));
    host.shutdown();
}

#[test]
fn keyframe_request_produces_a_keyframe() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(320, 240, 60, 1_000);
    let ack = peer.ack();
    peer.wait_for(Duration::from_secs(2), |p| {
        matches!(p, Packet::Video(h, _) if h.frame_id > 5).then_some(())
    })
    .expect("no video");
    peer.send(|b| {
        Feedback {
            session_id: ack.session_id,
            request_keyframe: true,
            ..Feedback::default()
        }
        .encode(b)
    });
    let key = peer.wait_for(Duration::from_secs(2), |p| match p {
        Packet::Video(h, _) if h.keyframe && h.frame_id > 5 => Some(h),
        _ => None,
    });
    assert!(key.is_some(), "no keyframe after request");
    host.shutdown();
}

#[test]
fn reported_loss_raises_fec_redundancy() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(640, 360, 60, 8_000);
    let ack = peer.ack();
    let ratio = |h: &VideoHeader| f32::from(h.recovery_shards) / f32::from(h.data_shards);
    let before = peer
        .wait_for(Duration::from_secs(2), |p| match p {
            Packet::Video(h, _) if h.data_shards >= 10 => Some(ratio(&h)),
            _ => None,
        })
        .expect("no video");
    for _ in 0..20 {
        peer.send(|b| {
            Feedback {
                session_id: ack.session_id,
                packets_received: 80,
                packets_lost: 20,
                ..Feedback::default()
            }
            .encode(b)
        });
    }
    let after = peer
        .wait_for(Duration::from_secs(2), |p| match p {
            Packet::Video(h, _) if h.data_shards >= 10 && ratio(&h) > before + 0.1 => {
                Some(ratio(&h))
            }
            _ => None,
        })
        .expect("redundancy did not rise");
    // Capped at 50 %, plus rounding up to whole shards.
    assert!(after <= 0.50 + 0.1, "{after}");
    host.shutdown();
}

#[test]
fn congestion_lowers_the_bitrate() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(640, 360, 60, 8_000);
    let ack = peer.ack();
    // Median frame size over the next ~30 frames.
    let frame_size = |peer: &mut Peer| {
        let mut sizes = Vec::new();
        while sizes.len() < 30 {
            if let Some(h) = peer.next_video(Duration::from_secs(2))
                && !h.keyframe
                && h.shard_index == 0
            {
                sizes.push(h.frame_len);
            }
        }
        sizes.sort_unstable();
        sizes[sizes.len() / 2]
    };
    let before = frame_size(&mut peer);
    // Congestion for three seconds: heavy loss, frames lost despite FEC.
    for _ in 0..30 {
        peer.send(|b| {
            Feedback {
                session_id: ack.session_id,
                packets_received: 70,
                packets_lost: 30,
                frames_dropped: 1,
                ..Feedback::default()
            }
            .encode(b)
        });
        std::thread::sleep(Duration::from_millis(100));
    }
    let after = frame_size(&mut peer);
    assert!(
        f64::from(after) < f64::from(before) * 0.6,
        "{before} → {after} bytes per frame"
    );
    host.shutdown();
}

#[test]
fn shutdown_sends_bye_to_the_peer() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(320, 240, 30, 500);
    let ack = peer.ack();
    host.shutdown();
    let bye = peer.wait_for(Duration::from_secs(2), |p| match p {
        Packet::Bye(b) => Some(b),
        _ => None,
    });
    assert_eq!(bye.map(|b| b.session_id), Some(ack.session_id));
}

#[test]
fn loss_injection_drops_video() {
    let host = Host::start(HostConfig {
        loss: 1.0,
        ..HostConfig::default()
    });
    let mut peer = Peer::new(host.addr);
    peer.hello(320, 240, 60, 1_000);
    peer.ack();
    assert!(peer.next_video(Duration::from_millis(500)).is_none());
    host.shutdown();
}

#[test]
fn bind_failure_is_reported() {
    let taken = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let err = HostAgent::bind(HostConfig {
        bind: addr.clone(),
        ..HostConfig::default()
    })
    .err()
    .expect("bind should fail");
    assert!(format!("{err:#}").contains(&addr));
}

#[test]
fn stats_count_the_pipeline() {
    use fernsicht_host_agent::HostStats;
    let agent = HostAgent::bind(HostConfig {
        bind: "127.0.0.1:0".into(),
        ..HostConfig::default()
    })
    .unwrap();
    let addr = agent.local_addr().unwrap();
    let stats = agent.stats();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = stop.clone();
        std::thread::spawn(move || agent.run(stop))
    };
    let mut peer = Peer::new(addr);
    peer.hello(320, 240, 60, 1_000);
    peer.ack();
    peer.wait_for(Duration::from_secs(2), |p| {
        matches!(p, Packet::Video(h, _) if h.frame_id >= 20).then_some(())
    })
    .expect("no video");
    stop.store(true, Ordering::Relaxed);
    thread.join().unwrap().unwrap();

    let get = HostStats::get;
    assert_eq!(get(&stats.sessions), 1);
    assert!(get(&stats.frames_captured) >= get(&stats.frames_encoded));
    assert!(get(&stats.frames_encoded) >= get(&stats.frames_sent));
    assert!(get(&stats.frames_sent) >= 20);
    assert!(get(&stats.keyframes_encoded) >= 1);
}

/// A session whose capture or encoder cannot start is refused (no ack), and
/// the host keeps serving.
fn assert_session_refused(cfg: HostConfig) {
    let host = Host::start(cfg);
    let mut peer = Peer::new(host.addr);
    peer.hello(640, 360, 60, 2_000);
    let ack = peer.wait_for(Duration::from_millis(500), |p| match p {
        Packet::HelloAck(a) => Some(a),
        _ => None,
    });
    assert!(ack.is_none(), "session must be refused, got {ack:?}");
    peer.send(|b| {
        ClockPing {
            seq: 1,
            client_send_us: 1,
        }
        .encode(b)
    });
    assert!(
        peer.wait_for(Duration::from_secs(2), |p| match p {
            Packet::ClockPong(_) => Some(()),
            _ => None,
        })
        .is_some(),
        "host stopped answering"
    );
    host.shutdown();
}

#[test]
fn session_without_capture_is_refused() {
    assert_session_refused(HostConfig {
        capture: CaptureKind::Kms {
            card: Some("/dev/dri/card-does-not-exist".into()),
            connector: None,
        },
        ..HostConfig::default()
    });
}

#[test]
fn session_without_encoder_is_refused() {
    assert_session_refused(HostConfig {
        encoder: EncoderKind::Vaapi {
            render_node: "/dev/dri/renderD-does-not-exist".into(),
        },
        ..HostConfig::default()
    });
}

#[test]
fn pointer_is_sent_with_its_shape_and_repeated() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(640, 360, 60, 2_000);
    let ack = peer.ack();
    // The test pattern's arrow: 12×19, one piece.
    let (shape, piece) = peer
        .wait_for(Duration::from_secs(2), |p| match p {
            Packet::CursorShape(s, data) => Some((s, data.to_vec())),
            _ => None,
        })
        .expect("no cursor shape");
    assert_eq!(shape.session_id, ack.session_id);
    assert_eq!((shape.width, shape.height, shape.offset), (12, 19, 0));
    assert_eq!(piece.len(), 12 * 19 * 4);
    assert_eq!(&piece[..4], &[0, 0, 0, 255], "arrow tip");

    let positions: Vec<Cursor> = (0..5)
        .filter_map(|_| {
            peer.wait_for(Duration::from_secs(1), |p| match p {
                Packet::Cursor(c) => Some(c),
                _ => None,
            })
        })
        .collect();
    assert_eq!(positions.len(), 5, "a position with every frame");
    for c in &positions {
        assert!(c.visible);
        assert_eq!(c.session_id, ack.session_id);
        assert_eq!(c.shape_serial, shape.serial);
        assert_eq!((c.screen_width, c.screen_height), (640, 360));
    }
    assert!(
        positions
            .windows(2)
            .any(|w| (w[0].x, w[0].y) != (w[1].x, w[1].y)),
        "the test pattern's pointer moves"
    );
    // The unchanged shape comes again, so a lost piece heals.
    assert!(
        peer.wait_for(Duration::from_secs(4), |p| matches!(
            p,
            Packet::CursorShape(..)
        )
        .then_some(()))
            .is_some(),
        "shape not repeated"
    );
    host.shutdown();
}

fn send_input(peer: &Peer, session_id: u32, events: &[(u32, InputEvent)]) {
    peer.send(|b| InputHeader::encode(session_id, events, b));
}

fn input_ack(peer: &mut Peer) -> u32 {
    peer.wait_for(Duration::from_secs(2), |p| match p {
        Packet::InputAck(a) => Some(a.seq),
        _ => None,
    })
    .expect("no input ack")
}

fn key(code: u16, pressed: bool) -> InputEvent {
    InputEvent::Key { code, pressed }
}

#[test]
fn input_is_applied_once_in_order_and_acknowledged() {
    let recorder = Recorder::default();
    let host = Host::start(HostConfig {
        input: InputKind::Record(recorder.clone()),
        ..HostConfig::default()
    });
    let mut peer = Peer::new(host.addr);
    peer.hello(320, 240, 30, 500);
    let ack = peer.ack();
    let first = [(1, key(30, true)), (2, key(30, false))];
    send_input(&peer, ack.session_id, &first);
    assert_eq!(input_ack(&mut peer), 2);
    // The same packet again (the ack was "lost"): nothing applied twice.
    send_input(&peer, ack.session_id, &first);
    assert_eq!(input_ack(&mut peer), 2);
    // Old and new together, as the client resends: only the new applies.
    let next = [
        (2, key(30, false)),
        (3, InputEvent::MouseAbs { x: 100, y: 200 }),
        (
            4,
            InputEvent::Button {
                code: 0x110,
                pressed: true,
            },
        ),
    ];
    send_input(&peer, ack.session_id, &next);
    assert_eq!(input_ack(&mut peer), 4);
    // Wrong session: ignored, no ack.
    send_input(&peer, ack.session_id ^ 1, &[(5, key(31, true))]);
    assert!(
        peer.wait_for(Duration::from_millis(300), |p| matches!(
            p,
            Packet::InputAck(_)
        )
        .then_some(()))
            .is_none()
    );
    host.shutdown();
    assert_eq!(
        *recorder.0.lock().unwrap(),
        vec![
            key(30, true),
            key(30, false),
            InputEvent::MouseAbs { x: 100, y: 200 },
            InputEvent::Button {
                code: 0x110,
                pressed: true
            },
        ]
    );
}

#[test]
fn input_is_off_by_default_but_acknowledged() {
    let host = Host::default();
    let mut peer = Peer::new(host.addr);
    peer.hello(320, 240, 30, 500);
    let ack = peer.ack();
    send_input(&peer, ack.session_id, &[(1, key(30, true))]);
    // Acknowledged, so the client does not resend forever.
    assert_eq!(input_ack(&mut peer), 1);
    host.shutdown();
}

#[test]
fn input_from_a_stranger_is_ignored() {
    let recorder = Recorder::default();
    let host = Host::start(HostConfig {
        input: InputKind::Record(recorder.clone()),
        ..HostConfig::default()
    });
    let mut peer = Peer::new(host.addr);
    peer.hello(320, 240, 30, 500);
    let ack = peer.ack();
    let mut stranger = Peer::new(host.addr);
    send_input(&stranger, ack.session_id, &[(1, key(30, true))]);
    assert!(
        stranger
            .wait_for(Duration::from_millis(300), |p| matches!(
                p,
                Packet::InputAck(_)
            )
            .then_some(()))
            .is_none()
    );
    host.shutdown();
    assert!(recorder.0.lock().unwrap().is_empty());
}

// ── Secure sessions ─────────────────────────────────────────────────────

mod secure {
    use std::sync::Arc;

    use fernsicht_host_agent::{HostConfig, HostSecurity, InputKind};
    use fernsicht_input::Recorder;
    use fernsicht_proto::*;
    use fernsicht_secure::pairing::ClientPairing;
    use fernsicht_secure::session::{Initiator, Transport};
    use fernsicht_secure::{Identity, Trusted};

    use super::{Host, Peer};
    use std::time::Duration;

    fn security() -> Arc<HostSecurity> {
        Arc::new(HostSecurity::new(
            Identity::generate(),
            "zentrale",
            Trusted::default(),
            None,
        ))
    }

    /// Pairs `client` through the raw peer; returns the host's key.
    fn pair(
        peer: &mut Peer,
        client: &Identity,
        pin: &str,
    ) -> Result<fernsicht_secure::PublicKey, String> {
        let (mut p, m1) = ClientPairing::start(pin, client, "bazzite");
        peer.send(|b| {
            Pair {
                step: 1,
                message: &m1,
            }
            .encode(b)
        });
        let reply = peer
            .wait_for(Duration::from_secs(2), |p| match p {
                Packet::Pair(p) if p.step == 2 => Some(Ok(p.message.to_vec())),
                Packet::Reject(r) => Some(Err(format!("{r:?}"))),
                _ => None,
            })
            .ok_or("no answer")??;
        let m3 = p.on_reply(&reply).map_err(|e| e.to_string())?;
        peer.send(|b| {
            Pair {
                step: 3,
                message: &m3,
            }
            .encode(b)
        });
        let m4 = peer
            .wait_for(Duration::from_secs(2), |p| match p {
                Packet::Pair(p) if p.step == 4 => Some(p.message.to_vec()),
                _ => None,
            })
            .ok_or("no step 4")?;
        Ok(p.on_done(&m4).map_err(|e| e.to_string())?.key)
    }

    /// Handshake as `client`; returns the session's keys and the ack.
    fn connect(
        peer: &mut Peer,
        client: &Identity,
        host: &fernsicht_secure::PublicKey,
        ms: u64,
    ) -> Option<(Transport, HelloAck, Vec<u8>)> {
        let hello = Hello {
            width: 320,
            height: 240,
            fps: 30,
            bitrate_kbps: 500,
        };
        let (init, m1) = Initiator::start(client, host, &hello.encode_with_time(ms)).unwrap();
        peer.send(|b| {
            Handshake {
                reply: false,
                message: &m1,
            }
            .encode(b)
        });
        let m2 = peer.wait_for(Duration::from_secs(2), |p| match p {
            Packet::Handshake(h) if h.reply => Some(h.message.to_vec()),
            _ => None,
        })?;
        let (t, payload) = init.finish(&m2).unwrap();
        let Ok(Packet::HelloAck(ack)) = Packet::decode(&payload) else {
            panic!("no HelloAck inside")
        };
        Some((t, ack, m1))
    }

    fn seal(t: &Transport, session_id: u32, packet: &[u8]) -> Vec<u8> {
        let mut buf = [0u8; MAX_DATAGRAM];
        let (counter, n) = t.seal(packet, &mut buf[SealedHeader::LEN..]).unwrap();
        SealedHeader {
            session_id,
            counter,
        }
        .write(&mut buf);
        buf[..SealedHeader::LEN + n].to_vec()
    }

    #[test]
    fn paired_session_is_encrypted_end_to_end() {
        let sec = security();
        sec.open_pairing("424242");
        let recorder = Recorder::default();
        let host = Host::start(HostConfig {
            security: Some(sec.clone()),
            input: InputKind::Record(recorder.clone()),
            ..HostConfig::default()
        });
        let mut peer = Peer::new(host.addr);
        let client = Identity::generate();
        let host_key = pair(&mut peer, &client, "424242").unwrap();
        assert_eq!(host_key, sec.public_key());
        assert_eq!(sec.paired()[0].name, "bazzite");

        let (t, ack, _) = connect(&mut peer, &client, &host_key, 1).expect("no handshake answer");
        assert_eq!((ack.width, ack.height), (320, 240));
        // From now on everything arrives sealed, and opens to real packets.
        let mut kinds = std::collections::HashSet::new();
        let mut opened = [0u8; MAX_DATAGRAM];
        for _ in 0..200 {
            let Some((h, sealed)) = peer.wait_for(Duration::from_secs(1), |p| match p {
                Packet::Sealed(h, s) => Some((h, s.to_vec())),
                Packet::Video(..) | Packet::Cursor(..) | Packet::CursorShape(..) => {
                    panic!("plaintext from a secure session")
                }
                _ => None,
            }) else {
                break;
            };
            assert_eq!(h.session_id, ack.session_id);
            let n = t.open(h.counter, &sealed, &mut opened).expect("opens");
            kinds.insert(match Packet::decode(&opened[..n]).unwrap() {
                Packet::Video(..) => "video",
                Packet::Cursor(..) => "cursor",
                Packet::CursorShape(..) => "cursor shape",
                _ => "other",
            });
        }
        assert!(kinds.len() >= 2, "video and pointer inside");

        // Sealed input applies; the same sealed packet again does not.
        let mut b = [0u8; MAX_DATAGRAM];
        let n = InputHeader::encode(
            ack.session_id,
            &[(
                1,
                InputEvent::Key {
                    code: 30,
                    pressed: true,
                },
            )],
            &mut b,
        );
        let sealed_input = seal(&t, ack.session_id, &b[..n]);
        peer.sock.send(&sealed_input).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        peer.sock.send(&sealed_input).unwrap();
        // Plain input in a secure session is ignored.
        peer.sock.send(&b[..n]).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        host.shutdown();
        assert_eq!(
            *recorder.0.lock().unwrap(),
            vec![InputEvent::Key {
                code: 30,
                pressed: true
            }]
        );
    }

    #[test]
    fn unpaired_devices_are_refused() {
        let host = Host::start(HostConfig {
            security: Some(security()),
            ..HostConfig::default()
        });
        let mut peer = Peer::new(host.addr);
        // Plain Hello: ignored by a secure host.
        peer.hello(320, 240, 30, 500);
        assert!(
            peer.wait_for(Duration::from_millis(400), |p| matches!(
                p,
                Packet::HelloAck(_)
            )
            .then_some(()))
                .is_none()
        );
        // A handshake from an unknown key: "not paired".
        let stranger = Identity::generate();
        let host_key = Identity::generate().public; // wrong guess too
        let (_, m1) = Initiator::start(&stranger, &host_key, &[0u8; 22]).unwrap();
        peer.send(|b| {
            Handshake {
                reply: false,
                message: &m1,
            }
            .encode(b)
        });
        assert!(
            peer.wait_for(Duration::from_millis(400), |p| matches!(
                p,
                Packet::Handshake(_)
            )
            .then_some(()))
                .is_none(),
            "a handshake for another host is not answered"
        );
        host.shutdown();
    }

    #[test]
    fn known_host_key_but_unpaired_client_gets_a_hint() {
        let sec = security();
        let host = Host::start(HostConfig {
            security: Some(sec.clone()),
            ..HostConfig::default()
        });
        let mut peer = Peer::new(host.addr);
        let stranger = Identity::generate();
        let hello = Hello {
            width: 0,
            height: 0,
            fps: 30,
            bitrate_kbps: 0,
        };
        let (_, m1) =
            Initiator::start(&stranger, &sec.public_key(), &hello.encode_with_time(1)).unwrap();
        peer.send(|b| {
            Handshake {
                reply: false,
                message: &m1,
            }
            .encode(b)
        });
        let r = peer.wait_for(Duration::from_secs(2), |p| match p {
            Packet::Reject(r) => Some(r),
            _ => None,
        });
        assert_eq!(r, Some(RejectReason::NotPaired));
        host.shutdown();
    }

    #[test]
    fn pairing_needs_pairing_mode_and_the_right_pin() {
        let sec = security();
        let host = Host::start(HostConfig {
            security: Some(sec.clone()),
            ..HostConfig::default()
        });
        let mut peer = Peer::new(host.addr);
        let client = Identity::generate();
        assert_eq!(
            pair(&mut peer, &client, "123456").unwrap_err(),
            "PairingClosed"
        );

        sec.open_pairing("123456");
        assert_eq!(pair(&mut peer, &client, "654321").unwrap_err(), "wrong PIN");
        assert_eq!(pair(&mut peer, &client, "111111").unwrap_err(), "wrong PIN");
        assert_eq!(pair(&mut peer, &client, "222222").unwrap_err(), "wrong PIN");
        // Three wrong guesses close pairing mode, even for the right PIN.
        assert_eq!(
            pair(&mut peer, &client, "123456").unwrap_err(),
            "TooManyAttempts"
        );
        assert!(sec.paired().is_empty());
        host.shutdown();
    }

    #[test]
    fn handshakes_are_answered_once_and_replays_dropped() {
        let sec = security();
        sec.open_pairing("000000");
        let host = Host::start(HostConfig {
            security: Some(sec.clone()),
            ..HostConfig::default()
        });
        let mut peer = Peer::new(host.addr);
        let client = Identity::generate();
        let key = pair(&mut peer, &client, "000000").unwrap();
        let (_, ack, m1) = connect(&mut peer, &client, &key, 1000).unwrap();
        // The same first message again (lost answer): same session.
        peer.send(|b| {
            Handshake {
                reply: false,
                message: &m1,
            }
            .encode(b)
        });
        assert!(
            peer.wait_for(Duration::from_secs(1), |p| {
                matches!(p, Packet::Handshake(h) if h.reply).then_some(())
            })
            .is_some()
        );
        // A new connection with an older clock (a replayed recording of an
        // earlier handshake would look like this): ignored.
        let mut other = Peer::new(host.addr);
        assert!(
            connect(&mut other, &client, &key, 999).is_none(),
            "older handshake must be dropped"
        );
        // A newer one replaces the session.
        let (_, ack2, _) = connect(&mut other, &client, &key, 2000).unwrap();
        assert_ne!(ack.session_id, ack2.session_id);
        host.shutdown();
    }
}
