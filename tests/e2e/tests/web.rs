//! The web viewer's host side against a WebRTC peer that plays the
//! browser (str0m): page and API over HTTP, the PIN, then picture, sound,
//! pointer, stats and input over WebRTC.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fernsicht_client::{Identity, Trusted};
use fernsicht_e2e::{Host, exclusive};
use fernsicht_host_agent::{AudioKind, HostConfig, HostSecurity, InputKind};
use fernsicht_input::Recorder;
use fernsicht_proto::{InputEvent, Packet};
use serde_json::{Value, json};
use str0m::change::SdpAnswer;
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, RtcConfig};

/// A minimal HTTP/1.1 request; returns status and body.
fn http(addr: SocketAddr, method: &str, path: &str, body: Option<&Value>) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, raw[split + 4..].to_vec())
}

fn json_of(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(body)))
}

fn host(sec: &Arc<HostSecurity>, recorder: &Recorder, root: Option<std::path::PathBuf>) -> Host {
    Host::start(HostConfig {
        security: Some(sec.clone()),
        web: Some("127.0.0.1:0".into()),
        web_root: root,
        input: InputKind::Record(recorder.clone()),
        audio: AudioKind::Tone,
        ..HostConfig::default()
    })
}

/// The browser's side: what it received so far.
#[derive(Default, Debug)]
struct Seen {
    video_frames: usize,
    audio_frames: usize,
    cursor: usize,
    stats: Option<Value>,
}

#[test]
fn a_browser_gets_picture_sound_and_pointer_and_types() {
    let _serial = exclusive();
    let sec = Arc::new(HostSecurity::new(
        Identity::generate(),
        "zentrale",
        Trusted::default(),
        None,
    ));
    let recorder = Recorder::default();
    let host = host(&sec, &recorder, None);
    let web = host.web_addr().expect("web server");

    let (status, body) = http(web, "GET", "/api/info", None);
    assert_eq!(status, 200);
    assert_eq!(
        json_of(&body),
        json!({"name": "zentrale", "pairing": false})
    );

    // The browser's offer: picture and sound to receive, one data channel.
    let mut rtc = RtcConfig::new().build(Instant::now());
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let local = socket.local_addr().unwrap();
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut change = rtc.sdp_api();
    change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    change.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
    let channel = change.add_channel("fernsicht".into());
    let (offer, pending) = change.apply().unwrap();
    let request = |pin: &str| json!({"pin": pin, "offer": offer});

    // No pairing open, then a wrong PIN: turned away.
    let (status, body) = http(web, "POST", "/api/connect", Some(&request("123456")));
    assert_eq!(
        (status, json_of(&body)),
        (403, json!({"error": "pairing-closed"}))
    );
    sec.open_pairing("482913");
    let (status, body) = http(web, "POST", "/api/connect", Some(&request("000000")));
    assert_eq!(
        (status, json_of(&body)),
        (403, json!({"error": "wrong-pin"}))
    );
    let (status, body) = http(web, "POST", "/api/connect", Some(&json!({"pin": "1"})));
    assert_eq!(
        (status, json_of(&body)),
        (400, json!({"error": "bad-request"}))
    );

    // The right PIN: an answer, and the PIN is used up.
    let (status, body) = http(web, "POST", "/api/connect", Some(&request("482913")));
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let reply = json_of(&body);
    assert!(reply["width"].as_u64().unwrap() > 0);
    assert!(sec.pairing_remaining().is_none());
    let answer: SdpAnswer = serde_json::from_value(reply["answer"].clone()).unwrap();
    rtc.sdp_api().accept_answer(pending, answer).unwrap();

    let mut seen = Seen::default();
    let mut sent_input = false;
    let mut buf = vec![0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(15);
    let done = |s: &Seen| {
        s.video_frames >= 10 && s.audio_frames >= 50 && s.cursor > 0 && s.stats.is_some()
    };
    while !done(&seen) {
        assert!(Instant::now() < deadline, "timed out: {seen:?}");
        let timeout = loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    socket.send_to(&t.contents, t.destination).unwrap();
                }
                Output::Event(e) => match e {
                    Event::MediaData(m) => match m.params.spec().codec {
                        str0m::format::Codec::Opus => seen.audio_frames += 1,
                        _ => seen.video_frames += 1,
                    },
                    Event::ChannelOpen(..) if !sent_input => {
                        let mut c = rtc.channel(channel).unwrap();
                        for msg in [
                            json!({"t": "m", "x": 32768, "y": 100}),
                            json!({"t": "k", "c": 30, "p": true}),
                            json!({"t": "k", "c": 30, "p": false}),
                        ] {
                            assert!(c.write(false, msg.to_string().as_bytes()).unwrap());
                        }
                        sent_input = true;
                    }
                    Event::ChannelData(d) if d.binary => {
                        if matches!(Packet::decode(&d.data), Ok(Packet::Cursor(_))) {
                            seen.cursor += 1;
                        }
                    }
                    Event::ChannelData(d) => {
                        let v: Value = serde_json::from_slice(&d.data).unwrap();
                        assert_eq!(v["type"], "stats");
                        seen.stats = Some(v);
                    }
                    _ => {}
                },
            }
        };
        let wait = timeout.saturating_duration_since(Instant::now());
        socket
            .set_read_timeout(Some(wait.max(Duration::from_millis(1))))
            .unwrap();
        let input = match socket.recv_from(&mut buf) {
            Ok((n, source)) => Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination: local,
                    contents: buf[..n].try_into().unwrap(),
                },
            ),
            Err(_) => Input::Timeout(Instant::now()),
        };
        rtc.handle_input(input).unwrap();
    }
    let stats = seen.stats.unwrap();
    assert!(stats["fps"].as_f64().unwrap() > 10.0, "{stats}");
    assert_eq!(stats["codec"], "Synthetisch");
    assert_eq!(
        *recorder.0.lock().unwrap(),
        [
            InputEvent::MouseAbs { x: 32768, y: 100 },
            InputEvent::Key {
                code: 30,
                pressed: true
            },
            InputEvent::Key {
                code: 30,
                pressed: false
            },
        ]
    );

    // The browser says goodbye: the session ends right away.
    rtc.channel(channel)
        .unwrap()
        .write(false, br#"{"t":"bye"}"#)
        .unwrap();
    let left = Instant::now();
    let sent = || {
        host.stats()
            .frames_sent
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    loop {
        // Keep the peer connection served so the goodbye gets out.
        while let Ok(Output::Transmit(t)) = rtc.poll_output() {
            socket.send_to(&t.contents, t.destination).unwrap();
        }
        let before = sent();
        std::thread::sleep(Duration::from_millis(300));
        if sent() == before {
            break;
        }
        assert!(
            left.elapsed() < Duration::from_secs(3),
            "the session keeps sending"
        );
    }
    host.stop();
}

#[test]
fn the_host_serves_the_viewer_page() {
    let _serial = exclusive();
    let root = std::env::temp_dir().join(format!("fernsicht-viewer-{}", std::process::id()));
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(
        root.join("index.html"),
        "<!doctype html><title>Fernsicht</title>",
    )
    .unwrap();
    std::fs::write(root.join("assets/app-1.js"), "console.log(1)").unwrap();
    let sec = Arc::new(HostSecurity::new(
        Identity::generate(),
        "zentrale",
        Trusted::default(),
        None,
    ));
    let host = host(&sec, &Recorder::default(), Some(root.clone()));
    let web = host.web_addr().unwrap();
    let (status, body) = http(web, "GET", "/", None);
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("<title>Fernsicht"));
    // App routes get the page (the viewer routes in the browser).
    assert_eq!(http(web, "GET", "/session/x?mode=gaming", None).0, 200);
    assert_eq!(
        http(web, "GET", "/assets/app-1.js", None).1,
        b"console.log(1)"
    );
    assert_eq!(http(web, "GET", "/assets/nope.js", None).0, 404);
    assert_eq!(http(web, "GET", "/../../etc/passwd", None).0, 404);
    assert_eq!(http(web, "DELETE", "/api/info", None).0, 404);
    host.stop();
    let _ = std::fs::remove_dir_all(&root);
}
