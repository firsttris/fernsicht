use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fernsicht_net::ReceiverStats;
use fernsicht_proto::{Codec, CodecSet, InputHeader, MAX_DATAGRAM, Packet};
use fernsicht_render::overlay::StreamInfo;

use super::*;
use crate::config::{CodecChoice, offered_codecs};
use crate::failure::{Failure, FailureCode, failure_of};
use crate::hosts::{
    default_state_dir, find_host, is_address, printable, remember_address, with_default_port,
};
use crate::input::{
    InputHandle, InputLink, chord_events, pad_events_while_focused, parse_keys_command,
    window_to_stream,
};
use crate::monitors::{Monitor, MonitorList, MonitorRequest, parse_monitor_command};
use crate::network::{RateWindow, ReceivedFrame, ratio};
use crate::present::{AppLine, AppStats, timings};

#[test]
fn gamepads_follow_the_focus() {
    let input = Arc::new(InputHandle::default());
    input.set_stream((100, 100));
    let focused = Arc::new(AtomicBool::new(false));
    let mut send = pad_events_while_focused(input.clone(), focused.clone());
    let a = |pressed| InputEvent::PadButton {
        pad: 0,
        code: 0x130,
        pressed,
    };
    send(a(true));
    assert!(input.queue.lock().unwrap().due(Instant::now()).is_none());
    focused.store(true, Ordering::Release);
    send(a(true));
    let mut q = input.queue.lock().unwrap();
    assert_eq!(q.due(Instant::now()).unwrap(), [(1, a(true))]);
}

#[test]
fn monitor_requests_are_resolved_against_the_list() {
    let l = MonitorList {
        current: 1,
        list: [
            ("DP-2", 2560, 1440),
            ("DP-1", 2560, 1440),
            ("HDMI-A-1", 1920, 1080),
        ]
        .map(|(name, width, height)| Monitor {
            name: name.into(),
            width,
            height,
        })
        .into(),
    };
    assert_eq!(l.resolve(MonitorRequest::Next), Some(2));
    assert_eq!(l.resolve(MonitorRequest::Previous), Some(0));
    assert_eq!(l.resolve(MonitorRequest::Index(2)), Some(2));
    assert_eq!(l.resolve(MonitorRequest::Index(3)), None);
    let last = MonitorList {
        current: 2,
        ..l.clone()
    };
    assert_eq!(last.resolve(MonitorRequest::Next), Some(0), "wraps around");
    assert_eq!(MonitorList::default().resolve(MonitorRequest::Next), None);
    let v = serde_json::to_value(&l).unwrap();
    assert_eq!(v["current"], 1);
    assert_eq!(v["list"][2]["name"], "HDMI-A-1");
    assert_eq!(v["list"][0]["width"], 2560);

    assert_eq!(
        parse_monitor_command("monitor 2"),
        Some(MonitorRequest::Index(2))
    );
    assert_eq!(
        parse_monitor_command("monitor next"),
        Some(MonitorRequest::Next)
    );
    assert_eq!(
        parse_monitor_command("monitor previous"),
        Some(MonitorRequest::Previous)
    );
    for bad in ["monitor", "monitor 8", "monitor -1", "monitor x", "keys 1"] {
        assert_eq!(parse_monitor_command(bad), None, "{bad:?}");
    }
}

#[test]
fn key_combinations_from_the_menu() {
    // Ctrl+Alt+Del
    assert_eq!(
        parse_keys_command("keys 29,56,111"),
        Some(vec![29, 56, 111])
    );
    assert_eq!(parse_keys_command("keys 125"), Some(vec![125]));
    for bad in [
        "keys",
        "keys ",
        "keys 0",
        "keys 99999",
        "keys 29,29",
        "keys 1,2,3,4,5,6,7",
        // Mouse buttons are no keys: the host would refuse them.
        "keys 272",
        "keys 29,273",
        "keys a",
        "mute",
    ] {
        assert_eq!(parse_keys_command(bad), None, "{bad:?}");
    }
    let key = |code, pressed| InputEvent::Key { code, pressed };
    assert_eq!(
        chord_events(&[29, 56, 111]),
        [
            key(29, true),
            key(56, true),
            key(111, true),
            key(111, false),
            key(56, false),
            key(29, false)
        ]
    );
}

#[test]
fn auto_offers_hevc_only_where_it_is_decoded() {
    let none = |_: Codec| false;
    let hevc = |c: Codec| c == Codec::Hevc;
    let all = |_: Codec| true;
    let both = CodecSet::of(&[Codec::H264, Codec::Hevc]);
    // The GTX 1080: HEVC in hardware, no AV1.
    assert_eq!(offered_codecs(CodecChoice::Auto, hevc), both);
    assert_eq!(offered_codecs(CodecChoice::Auto, none), CodecSet::H264);
    assert_eq!(
        offered_codecs(CodecChoice::Auto, all),
        CodecSet::of(&[Codec::H264, Codec::Hevc, Codec::Av1])
    );
    assert_eq!(
        offered_codecs(CodecChoice::Av1, none),
        CodecSet::of(&[Codec::Av1])
    );
    // A choice is passed on as it is.
    assert_eq!(offered_codecs(CodecChoice::H264, hevc), CodecSet::H264);
    assert_eq!(
        offered_codecs(CodecChoice::Hevc, none),
        CodecSet::of(&[Codec::Hevc])
    );
}

#[test]
fn host_addresses_get_the_default_port() {
    assert_eq!(with_default_port("zentrale"), "zentrale:47800");
    assert_eq!(with_default_port("192.168.178.87"), "192.168.178.87:47800");
    assert_eq!(
        with_default_port("192.168.178.87:1234"),
        "192.168.178.87:1234"
    );
    assert_eq!(with_default_port("[fe80::1]:47800"), "[fe80::1]:47800");
    assert_eq!(with_default_port("[fe80::1]"), "[fe80::1]:47800");
    assert_eq!(with_default_port("fe80::1"), "fe80::1:47800");
}

#[test]
fn hosts_are_found_by_name_or_address() {
    let mut t = Trusted::default();
    t.add(Peer {
        name: "zentrale".into(),
        key: Identity::generate().public,
        address: Some("192.168.178.87:47800".into()),
        paired_at: 0,
    });
    assert!(find_host(&t, "zentrale").is_some());
    assert!(find_host(&t, "192.168.178.87").is_some());
    assert!(find_host(&t, "192.168.178.87:47800").is_some());
    assert!(find_host(&t, "192.168.178.88").is_none());
    assert!(find_host(&t, "bazzite").is_none());
}

#[test]
fn a_name_cannot_pose_as_an_address_or_another_device() {
    let peer = |name: &str, address: &str| Peer {
        name: name.into(),
        key: Identity::generate().public,
        address: Some(address.into()),
        paired_at: 0,
    };
    let mut t = Trusted::default();
    // Paired first: a host that calls itself like the other's address.
    t.add(peer("192.168.178.87:47800", "192.168.178.66:47800"));
    t.add(peer("zentrale", "192.168.178.87:47800"));
    let zentrale = t.peers[1].key;
    assert_eq!(
        find_host(&t, "192.168.178.87:47800").map(|p| p.key),
        Some(zentrale)
    );
    // The key picks for sure.
    assert_eq!(
        find_host(&t, &zentrale.fingerprint()).map(|p| p.key),
        Some(zentrale)
    );
    // Two devices with one name: neither, rather than the wrong one.
    t.add(peer("zentrale", "192.168.178.90:47800"));
    assert!(find_host(&t, "zentrale").is_none());
    assert!(is_address("192.168.178.87") && is_address("[fe80::1]:47800"));
    assert!(!is_address("zentrale.local"));
    assert_eq!(printable("ze\u{1b}[2Jntrale"), "ze\u{fffd}[2Jntrale");
}

#[test]
fn an_address_is_kept_only_after_a_session_with_it() {
    let key = Identity::generate().public;
    let mut t = Trusted::default();
    t.add(Peer {
        name: "zentrale".into(),
        key,
        address: Some("192.168.178.87:47800".into()),
        paired_at: 0,
    });
    // Nothing new, or not a paired host: nothing to keep.
    assert!(remember_address(&t, &key, "192.168.178.87:47800").is_none());
    let other = Identity::generate().public;
    assert!(remember_address(&t, &other, "192.168.178.66:47800").is_none());
    // A session at a new address (DHCP) went through: that one is kept.
    let t = remember_address(&t, &key, "192.168.178.90:47800").unwrap();
    assert_eq!(t.peers[0].address.as_deref(), Some("192.168.178.90:47800"));
}

#[test]
fn the_state_dir_follows_xdg() {
    let d = default_state_dir();
    assert!(d.ends_with("fernsicht"), "{d:?}");
}

#[test]
fn window_positions_map_onto_the_stream() {
    // Same aspect: corners are corners.
    assert_eq!(
        window_to_stream((0.0, 0.0), (1280, 720), (2560, 1440)),
        Some((0, 0))
    );
    assert_eq!(
        window_to_stream((1280.0, 720.0), (1280, 720), (2560, 1440)),
        Some((65535, 65535))
    );
    assert_eq!(
        window_to_stream((640.0, 360.0), (1280, 720), (2560, 1440)),
        Some((32768, 32768))
    );
    // 16:9 stream in a square window: bars above and below.
    let sq = (1000, 1000);
    assert_eq!(
        window_to_stream((500.0, 100.0), sq, (1920, 1080)),
        None,
        "on the bar"
    );
    assert_eq!(
        window_to_stream((500.0, 218.75), sq, (1920, 1080)),
        Some((32768, 0))
    );
    assert_eq!(
        window_to_stream((500.0, 781.25), sq, (1920, 1080)),
        Some((32768, 65535))
    );
    assert_eq!(window_to_stream((-1.0, 500.0), sq, (1920, 1080)), None);
}

#[test]
fn input_goes_out_the_moment_it_is_pushed() {
    let host = UdpSocket::bind("127.0.0.1:0").unwrap();
    host.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client.connect(host.local_addr().unwrap()).unwrap();
    let input = InputHandle::default();
    let key = InputEvent::Key {
        code: 30,
        pressed: true,
    };
    // Before the session: queued, nothing sent.
    input.push(key);
    let mut buf = [0u8; MAX_DATAGRAM];
    host.set_nonblocking(true).unwrap();
    assert!(host.recv(&mut buf).is_err());
    host.set_nonblocking(false).unwrap();
    // With the session: the next event goes at once, with the waiting one.
    input.connect(Some(InputLink {
        socket: Arc::new(client),
        session_id: 7,
        crypto: None,
    }));
    input.push(InputEvent::Key {
        code: 30,
        pressed: false,
    });
    let n = host.recv(&mut buf).expect("sent without the network loop");
    let Ok(Packet::Input(h, body)) = Packet::decode(&buf[..n]) else {
        panic!("not input")
    };
    assert_eq!(h.session_id, 7);
    assert_eq!(InputHeader::events(body).count(), 2);
    // After the session: nothing goes out any more.
    input.connect(None);
    input.push(key);
    host.set_nonblocking(true).unwrap();
    assert!(host.recv(&mut buf).is_err());
}

#[test]
fn the_input_handle_waits_for_the_stream_size() {
    let h = InputHandle::default();
    assert_eq!(h.pointer_at((10.0, 10.0), (100, 100)), None);
    h.set_stream((100, 100));
    assert!(matches!(
        h.pointer_at((50.0, 50.0), (100, 100)),
        Some(InputEvent::MouseAbs { .. })
    ));
}

#[test]
fn rate_window_publishes_ratios() {
    let mut w = RateWindow::new();
    w.bytes = 1_000_000;
    w.frames = 60;
    w.add(&ReceiverStats {
        packets_received: 990,
        packets_lost: 10,
        frames_completed: 59,
        frames_dropped: 1,
        ..ReceiverStats::default()
    });
    let mut info = StreamInfo::default();
    w.publish(&mut info, Codec::H264, Some(500));
    assert_eq!(info.codec, Codec::H264);
    assert!((info.loss_before_fec - 0.01).abs() < 1e-6);
    assert!((info.loss_after_fec - 1.0 / 60.0).abs() < 1e-6);
    assert_eq!(info.rtt_us, Some(500));
    assert!(info.fps > 0.0 && info.bitrate_bps > 0);
    assert_eq!(ratio(1, 0), 0.0);
}

#[test]
fn frame_timings_are_put_on_the_clients_clock() {
    let mut rf = ReceivedFrame::empty();
    rf.header.capture_us = 10_000;
    rf.header.capture_ready_delta_us = 1_000;
    rf.header.encoded_delta_us = 3_000;
    rf.completed_us = 9_000;
    // The host's clock runs 4 ms ahead of ours.
    rf.offset_us = 4_000;
    let t = timings(&rf, 11_000, 12_500);
    assert_eq!(
        (t.captured, t.capture_ready, t.encoded),
        (6_000, 7_000, 9_000)
    );
    assert_eq!(
        (t.received, t.decoded, t.presented),
        (9_000, 11_000, 12_500)
    );
}

#[test]
fn the_app_gets_the_overlay_and_the_monitors_in_one_object() {
    let line = AppStats {
        overlay: fernsicht_render::overlay::Stats {
            codec: "AV1".into(),
            fps: 60.0,
            ..Default::default()
        },
        monitors: None,
    };
    let v = serde_json::to_value(&line).unwrap();
    assert_eq!(v["codec"], "AV1");
    assert_eq!(v["stagesUs"]["network"], 0);
    assert!(
        v.get("monitors").is_none(),
        "left out until the host lists them"
    );
    let with = AppStats {
        monitors: Some(MonitorList {
            current: 0,
            list: vec![Monitor {
                name: "DP-1".into(),
                width: 2560,
                height: 1440,
            }],
        }),
        ..line
    };
    let text = serde_json::to_string(&with).unwrap();
    assert!(text.contains(r#""monitors":{"current":0,"list":[{"name":"DP-1""#));
    assert_eq!(serde_json::from_str::<AppStats>(&text).unwrap(), with);
}

#[test]
fn failures_keep_their_code_through_context_and_the_app_line() {
    let e = anyhow::Error::new(
        Failure::rejected(fernsicht_proto::RejectReason::NotPaired).at("192.0.2.1:47800"),
    )
    .context("streaming");
    let f = failure_of(&e);
    assert_eq!(f.code, FailureCode::NotPaired);
    assert!(
        f.message
            .starts_with("streaming: 192.0.2.1:47800: this device is not paired"),
        "{f}"
    );
    assert_eq!(failure_of(&anyhow::anyhow!("odd")).code, FailureCode::Other);

    let line = serde_json::to_string(&AppLine::Failed { failure: f.clone() }).unwrap();
    assert!(
        line.starts_with(r#"{"failure":{"code":"not-paired","#),
        "{line}"
    );
    assert_eq!(
        serde_json::from_str::<AppLine>(&line).unwrap(),
        AppLine::Failed { failure: f }
    );
    let stats = serde_json::to_string(&AppStats {
        overlay: Default::default(),
        monitors: None,
    })
    .unwrap();
    assert!(matches!(
        serde_json::from_str::<AppLine>(&stats).unwrap(),
        AppLine::Stats(_)
    ));
}
