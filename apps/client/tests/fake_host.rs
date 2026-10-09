//! The client against a scripted fake host: handshake, clock sync,
//! feedback, timeouts and corrupt streams.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fernsicht_capture::{Frame, PixelFormat};
use fernsicht_client::{ClientConfig, RunSummary, run};
use fernsicht_codec::synthetic::SyntheticEncoder;
use fernsicht_codec::{EncodedFrame, Encoder};
use fernsicht_core::latency::Stage;
use fernsicht_core::now_us;
use fernsicht_net::{FecConfig, FrameMeta, Packetizer};
use fernsicht_proto::*;

/// How the fake host behaves.
#[derive(Clone)]
struct Script {
    /// Ignore this many Hellos before answering.
    ignore_hellos: u32,
    /// Host clock = client clock + this (same process, same epoch).
    clock_offset_us: i64,
    /// Send video at ~60 fps once the session is up.
    stream: bool,
    /// Flip a byte in every n-th frame (0 = never).
    corrupt_every: u32,
    /// Send `Bye` after this many frames.
    bye_after: Option<u32>,
    /// Stop answering anything after this long.
    go_silent_after: Option<Duration>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            ignore_hellos: 0,
            clock_offset_us: 0,
            stream: true,
            corrupt_every: 0,
            bye_after: None,
            go_silent_after: None,
        }
    }
}

#[derive(Default)]
struct Observed {
    hellos: u32,
    pings: u32,
    feedback: Vec<Feedback>,
    byes: Vec<Bye>,
}

struct FakeHost {
    addr: String,
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Observed>>,
    thread: Option<JoinHandle<()>>,
}

const SESSION: u32 = 0xABCD_0001;

impl FakeHost {
    fn start(script: Script) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(2)))
            .unwrap();
        let addr = sock.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Observed::default()));
        let thread = {
            let (stop, seen) = (stop.clone(), seen.clone());
            std::thread::spawn(move || serve(sock, script, &stop, &seen))
        };
        Self {
            addr,
            stop,
            seen,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> Observed {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

fn serve(sock: UdpSocket, script: Script, stop: &AtomicBool, seen: &Mutex<Observed>) {
    let started = Instant::now();
    let host_now = || (now_us() as i64 + script.clock_offset_us) as u64;
    let mut buf = [0u8; 2048];
    let mut out = [0u8; MAX_DATAGRAM];
    let mut peer = None;
    let mut encoder = SyntheticEncoder::new(4_000, 60);
    let mut packetizer = Packetizer::new(FecConfig::default()).unwrap();
    let mut encoded = EncodedFrame::default();
    let mut frame = Frame::new(320, 240, PixelFormat::Nv12);
    let mut frame_id = 0u32;
    let mut next_frame = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let silent = script
            .go_silent_after
            .is_some_and(|d| started.elapsed() > d);
        if let Ok((n, from)) = sock.recv_from(&mut buf)
            && !silent
        {
            let recv = host_now();
            let mut seen = seen.lock().unwrap();
            match Packet::decode(&buf[..n]) {
                Ok(Packet::Hello(_)) => {
                    seen.hellos += 1;
                    if seen.hellos > script.ignore_hellos {
                        peer = Some(from);
                        let ack = HelloAck {
                            session_id: SESSION,
                            width: 320,
                            height: 240,
                            fps: 60,
                            codec: Codec::Synthetic,
                        };
                        let n = ack.encode(&mut out);
                        sock.send_to(&out[..n], from).unwrap();
                    }
                }
                Ok(Packet::ClockPing(p)) => {
                    seen.pings += 1;
                    let pong = ClockPong {
                        seq: p.seq,
                        client_send_us: p.client_send_us,
                        host_recv_us: recv,
                        host_send_us: host_now(),
                    };
                    let n = pong.encode(&mut out);
                    sock.send_to(&out[..n], from).unwrap();
                }
                Ok(Packet::Feedback(f)) => {
                    if f.request_keyframe {
                        encoder.request_keyframe();
                    }
                    seen.feedback.push(f);
                }
                Ok(Packet::Bye(b)) => seen.byes.push(b),
                _ => {}
            }
        }

        let Some(peer) = peer else { continue };
        if silent || !script.stream || Instant::now() < next_frame {
            continue;
        }
        if script.bye_after.is_some_and(|n| frame_id >= n) {
            let n = Bye {
                session_id: SESSION,
            }
            .encode(&mut out);
            sock.send_to(&out[..n], peer).unwrap();
            return;
        }
        next_frame += Duration::from_micros(16_667);
        frame.seq = u64::from(frame_id);
        frame.capture_us = host_now();
        frame.ready_us = frame.capture_us + 500;
        encoder.encode(&frame, &mut encoded).unwrap();
        if script.corrupt_every > 0 && frame_id % script.corrupt_every == script.corrupt_every - 1 {
            let mid = encoded.data.len() / 2;
            encoded.data[mid] ^= 0xFF;
        }
        let meta = FrameMeta {
            session_id: SESSION,
            frame_id,
            keyframe: encoded.keyframe,
            capture_us: frame.capture_us,
            capture_ready_delta_us: 500,
            encoded_delta_us: 1_500,
        };
        for pkt in packetizer.packetize(&meta, &encoded.data).unwrap() {
            sock.send_to(pkt, peer).unwrap();
        }
        frame_id += 1;
    }
}

fn client(host: &FakeHost, secs: f32) -> anyhow::Result<RunSummary> {
    run(
        ClientConfig {
            host: host.addr.clone(),
            duration: Some(Duration::from_secs_f32(secs)),
            host_timeout: Duration::from_millis(600),
            ..ClientConfig::default()
        },
        Arc::new(AtomicBool::new(false)),
    )
}

#[test]
fn handshake_and_stream() {
    let host = FakeHost::start(Script::default());
    let s = client(&host, 1.5).unwrap();
    let seen = host.finish();
    assert_eq!(s.session_id, Some(SESSION));
    assert_eq!(s.codec, Some(Codec::Synthetic));
    assert!(s.frames_presented >= 45, "{s:?}");
    assert_eq!(s.keyframes_decoded, 1, "{s:?}");
    assert_eq!(s.decode_errors, 0);
    assert_eq!(
        seen.hellos, 1,
        "client must stop sending Hello after the ack"
    );
    // On a clean run the client says goodbye with the right session.
    assert_eq!(seen.byes.len(), 1);
    assert_eq!(seen.byes[0].session_id, SESSION);
}

#[test]
fn hello_is_retried_until_acked() {
    let host = FakeHost::start(Script {
        ignore_hellos: 2,
        ..Script::default()
    });
    let s = client(&host, 1.5).unwrap();
    let seen = host.finish();
    assert_eq!(seen.hellos, 3);
    assert_eq!(s.session_id, Some(SESSION));
    assert!(s.frames_presented > 0);
}

#[test]
fn clock_offset_is_estimated_and_used_for_latency() {
    // Host clock runs 5 s ahead. Without sync, every stage would be ~5 s.
    let host = FakeHost::start(Script {
        clock_offset_us: 5_000_000,
        ..Script::default()
    });
    let s = client(&host, 1.5).unwrap();
    let seen = host.finish();
    assert!(seen.pings >= 10, "fast pings while syncing");
    assert!(
        (s.clock_offset_us - 5_000_000).abs() < 2_000,
        "{}",
        s.clock_offset_us
    );
    assert!(s.rtt_us.unwrap() < 5_000);
    // Fake host stamps capture_ready = +0.5 ms and encoded = +1.5 ms.
    let capture = s.stage(Stage::Capture);
    assert!((400..=2_500).contains(&capture.avg), "{capture:?}");
    let encode = s.stage(Stage::Encode);
    assert!((900..=3_000).contains(&encode.avg), "{encode:?}");
    assert!(s.total.avg < 50_000, "{:?}", s.total);
}

#[test]
fn feedback_reports_the_session_and_asks_for_a_keyframe_until_one_arrives() {
    let host = FakeHost::start(Script::default());
    client(&host, 1.0).unwrap();
    let seen = host.finish();
    assert!(seen.feedback.len() >= 5, "{}", seen.feedback.len());
    assert!(seen.feedback.iter().all(|f| f.session_id == SESSION));
    let last = seen.feedback.last().unwrap();
    assert!(
        !last.request_keyframe,
        "keyframe arrived, request must stop"
    );
    let received: u32 = seen.feedback.iter().map(|f| f.packets_received).sum();
    assert!(received > 0);
    assert_eq!(
        seen.feedback.iter().map(|f| f.frames_dropped).sum::<u32>(),
        0
    );
}

#[test]
fn corrupt_frames_are_counted_not_shown() {
    let host = FakeHost::start(Script {
        corrupt_every: 5,
        ..Script::default()
    });
    let s = client(&host, 1.5).unwrap();
    host.finish();
    assert!(s.decode_errors >= 3, "{s:?}");
    // After each corrupt frame the client waits for a keyframe it requests.
    assert!(s.keyframes_decoded >= 3, "{s:?}");
    assert!(s.frames_presented > 0);
    assert_eq!(
        s.frames_presented
            + s.frames_skipped
            + s.decode_errors
            + s.frames_awaiting_keyframe
            + s.frames_overflowed,
        u64::from(s.receiver.frames_completed),
        "every completed frame is accounted for exactly once: {s:?}"
    );
}

#[test]
fn host_bye_ends_the_run_early() {
    let host = FakeHost::start(Script {
        bye_after: Some(10),
        ..Script::default()
    });
    let started = Instant::now();
    let s = client(&host, 10.0).unwrap();
    host.finish();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(s.frames_presented >= 5, "{s:?}");
}

#[test]
fn silent_host_is_an_error() {
    let host = FakeHost::start(Script {
        go_silent_after: Some(Duration::from_millis(300)),
        ..Script::default()
    });
    let err = client(&host, 10.0).unwrap_err();
    host.finish();
    assert!(err.to_string().contains("no packets"), "{err}");
}

#[test]
fn unreachable_host_times_out() {
    // Bound but never answers.
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let err = run(
        ClientConfig {
            host: sock.local_addr().unwrap().to_string(),
            host_timeout: Duration::from_millis(300),
            ..ClientConfig::default()
        },
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err();
    assert!(err.to_string().contains("no packets"));
}

#[test]
fn stop_flag_ends_the_run() {
    let host = FakeHost::start(Script::default());
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let (stop, addr) = (stop.clone(), host.addr.clone());
        std::thread::spawn(move || {
            run(
                ClientConfig {
                    host: addr,
                    ..ClientConfig::default()
                },
                stop,
            )
        })
    };
    std::thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::Relaxed);
    let s = handle.join().unwrap().unwrap();
    host.finish();
    assert!(s.frames_presented > 0);
}

#[test]
fn invalid_host_address_is_an_error() {
    let err = run(
        ClientConfig {
            host: "not a host".into(),
            ..ClientConfig::default()
        },
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("connect"));
}
