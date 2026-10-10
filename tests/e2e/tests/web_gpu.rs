//! The web viewer with a real GPU encoder: a WebRTC peer plays a browser
//! that offers AV1 and H.264, like Chrome and Firefox. The host must send
//! AV1 where its GPU encodes it (RX 7800 XT) and H.264 where not (GTX
//! 1080), and what arrives must decode. `FERNSICHT_GPU=amd|intel|nvidia`;
//! skips otherwise.
#![cfg(any(feature = "vaapi", feature = "nvidia"))]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fernsicht_client::{Identity, Trusted};
use fernsicht_codec::{DecodedFrame, Decoder};
use fernsicht_e2e::{Host, exclusive};
use fernsicht_host_agent::{EncoderKind, HostConfig, HostSecurity};
use fernsicht_proto::Codec;
use serde_json::{Value, json};
use str0m::change::SdpAnswer;
use str0m::format::Codec as RtcCodec;
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, RtcConfig};

struct Gpu {
    encoder: EncoderKind,
    /// The codec the host must pick for a browser offering AV1 and H.264.
    expected: Codec,
    decoder: Box<dyn Fn(Codec) -> Box<dyn Decoder>>,
}

fn gpu() -> Option<Gpu> {
    match std::env::var("FERNSICHT_GPU").as_deref() {
        #[cfg(feature = "vaapi")]
        Ok("amd") => {
            let node = std::env::var("FERNSICHT_RENDER_NODE")
                .unwrap_or_else(|_| "/dev/dri/renderD128".into());
            Some(Gpu {
                encoder: EncoderKind::Vaapi {
                    render_node: node.clone(),
                },
                // VCN 4 encodes AV1.
                expected: Codec::Av1,
                decoder: Box::new(move |c| {
                    Box::new(fernsicht_codec::vaapi::VaapiDecoder::for_codec(&node, c).unwrap())
                }),
            })
        }
        #[cfg(feature = "nvidia")]
        Ok("nvidia") => Some(Gpu {
            encoder: EncoderKind::Nvenc { gpu: 0 },
            // Pascal has no AV1 encoder: the host falls back to H.264.
            expected: Codec::H264,
            decoder: Box::new(|c| {
                Box::new(fernsicht_codec::nvidia::NvdecDecoder::for_codec(0, c).unwrap())
            }),
        }),
        other => {
            eprintln!("skipped: no hardware path for FERNSICHT_GPU={other:?} in this build");
            None
        }
    }
}

fn http(addr: SocketAddr, path: &str, body: &Value) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let body = body.to_string();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_slice(&raw[split + 4..]).unwrap())
}

#[test]
fn the_browser_gets_the_best_codec_the_gpu_encodes() {
    let Some(gpu) = gpu() else { return };
    let _serial = exclusive();
    let sec = Arc::new(HostSecurity::new(
        Identity::generate(),
        "zentrale",
        Trusted::default(),
        None,
    ));
    let host = Host::start(HostConfig {
        security: Some(sec.clone()),
        web: Some("127.0.0.1:0".into()),
        encoder: gpu.encoder.clone(),
        ..HostConfig::default()
    });
    let web = host.web_addr().expect("web server");

    // What Chrome and Firefox offer for video, in this order.
    let mut rtc = RtcConfig::new()
        .clear_codecs()
        .enable_opus(true, false)
        .enable_av1(true)
        .enable_h264(true)
        .build(Instant::now());
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let local = socket.local_addr().unwrap();
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut change = rtc.sdp_api();
    change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    change.add_channel("fernsicht".into());
    let (offer, pending) = change.apply().unwrap();
    sec.open_pairing("482913");
    let (status, reply) = http(
        web,
        "/api/connect",
        &json!({"pin": "482913", "offer": offer}),
    );
    assert_eq!(status, 200, "{reply}");
    let answer: SdpAnswer = serde_json::from_value(reply["answer"].clone()).unwrap();
    rtc.sdp_api().accept_answer(pending, answer).unwrap();

    let mut frames: Vec<(RtcCodec, Vec<u8>)> = Vec::new();
    let mut stats: Option<Value> = None;
    let mut buf = vec![0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(20);
    while frames.len() < 60 || stats.is_none() {
        assert!(
            Instant::now() < deadline,
            "timed out: {} frames, stats {stats:?}",
            frames.len()
        );
        let timeout = loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(t) => break t,
                Output::Transmit(t) => {
                    socket.send_to(&t.contents, t.destination).unwrap();
                }
                Output::Event(Event::MediaData(m)) => {
                    frames.push((m.params.spec().codec, m.data.to_vec()));
                }
                Output::Event(Event::ChannelData(d)) if !d.binary => {
                    stats = Some(serde_json::from_slice(&d.data).unwrap());
                }
                Output::Event(_) => {}
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
    host.stop();

    let (want_rtc, label) = match gpu.expected {
        Codec::Av1 => (RtcCodec::Av1, "AV1"),
        _ => (RtcCodec::H264, "H.264"),
    };
    assert!(
        frames.iter().all(|(c, _)| *c == want_rtc),
        "expected {label}, got {:?}",
        frames.iter().map(|(c, _)| c).collect::<Vec<_>>()
    );
    assert_eq!(stats.unwrap()["codec"], label);

    // What the browser received decodes, from the first frame on (the
    // stream starts with a keyframe).
    let mut dec = (gpu.decoder)(gpu.expected);
    let mut d = DecodedFrame::default();
    let mut decoded = 0;
    for (i, (_, data)) in frames.iter().enumerate() {
        dec.decode(data, &mut d)
            .unwrap_or_else(|e| panic!("frame {i} of {label}: {e}"));
        decoded += 1;
    }
    assert!(decoded >= 60 && d.width > 0 && d.height > 0, "{d:?}");
    println!(
        "web viewer: {label}, {decoded} frames decoded at {}×{}",
        d.width, d.height
    );
}
