//! Protocol conformance: a raw UDP peer talks to a running host agent.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fernsicht_host_agent::{CaptureKind, EncoderKind, HostAgent, HostConfig};
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
