use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;
use fernsicht_capture::{Frame, FrameSource};
use fernsicht_codec::synthetic::SyntheticEncoder;
use fernsicht_codec::{CodecError, EncodedFrame, Encoder};
use fernsicht_core::Slot;
use fernsicht_proto::{Codec, CodecSet};
use fernsicht_secure::{Identity, Peer, Trusted};

use crate::backends::{
    NVIDIA, auto_encoder, choose_encoder, codec_candidates, default_bitrate_kbps, fit, frame_rate,
    make_encoder, render_nodes,
};
use crate::capture::switch_monitor;
use crate::config::{EncoderKind, HostDescription, SessionParams, describe_gpu};
use crate::encode::{Rebuild, encode_loop};
use crate::health::RepeatedError;
use crate::link::{Link, UdpLink};
use crate::monitors::monitor_list;
use crate::pointer::CursorSender;
use crate::security::HostSecurity;
use crate::shared::Shared;

#[test]
fn hevc_is_preferred_when_the_client_decodes_it() {
    let both = CodecSet::of(&[Codec::H264, Codec::Hevc]);
    assert_eq!(codec_candidates(both), vec![Codec::Hevc, Codec::H264]);
    assert_eq!(codec_candidates(CodecSet::H264), vec![Codec::H264]);
    assert_eq!(
        codec_candidates(CodecSet::of(&[Codec::Hevc])),
        vec![Codec::Hevc]
    );
    let all = CodecSet::of(&[Codec::H264, Codec::Av1, Codec::Hevc]);
    assert_eq!(
        codec_candidates(all),
        vec![Codec::Av1, Codec::Hevc, Codec::H264]
    );
}

/// A source with two monitors that records switches.
struct TwoMonitors {
    screen: fernsicht_capture::ScreenInfo,
    size: (u32, u32),
}

impl FrameSource for TwoMonitors {
    fn width(&self) -> u32 {
        self.size.0
    }
    fn height(&self) -> u32 {
        self.size.1
    }
    fn format(&self) -> fernsicht_capture::PixelFormat {
        fernsicht_capture::PixelFormat::Bgrx
    }
    fn next_frame(&mut self, _: &mut Frame) -> Result<(), fernsicht_capture::CaptureError> {
        Ok(())
    }
    fn screen(&self) -> Option<fernsicht_capture::ScreenInfo> {
        Some(self.screen.clone())
    }
    fn switch_to(&mut self, c: &str) -> Result<(), fernsicht_capture::CaptureError> {
        let i = self.screen.active.iter().position(|n| n == c).unwrap();
        self.screen.connector = c.into();
        self.size = self.screen.sizes[i];
        Ok(())
    }
}

fn shared() -> Shared {
    Shared {
        stats: Arc::default(),
        running: AtomicBool::new(true),
        keyframe_requested: AtomicBool::new(false),
        last_keyframe: Mutex::new(None),
        frame: Duration::from_micros(16_667),
        cursor_requested: AtomicBool::new(false),
        fec_loss: AtomicU32::new(0),
        target_kbps: AtomicU32::new(0),
        delivery_kbps: AtomicU32::new(0),
        nacks: crossbeam_channel::bounded(16),
        monitor_request: Default::default(),
        screen: Default::default(),
        screen_changes: AtomicU64::new(0),
        pointer: AtomicU64::new(crate::shared::NO_POINTER),
    }
}

#[test]
fn monitors_are_switched_on_request() {
    let mut source = TwoMonitors {
        screen: fernsicht_capture::ScreenInfo {
            connector: "DP-2".into(),
            active: vec!["DP-2".into(), "HDMI-A-1".into()],
            sizes: vec![(2560, 1440), (1920, 1080)],
        },
        size: (2560, 1440),
    };
    let shared = shared();
    shared.set_screen(source.screen());
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
    let link = Link::Udp(UdpLink {
        peer: socket.local_addr().unwrap(),
        socket,
        session_id: 1,
        crypto: None,
        confirmed: Arc::new(AtomicBool::new(true)),
        gso: Arc::new(AtomicBool::new(true)),
    });
    let mut cursor = CursorSender::new(link, 1, (2560, 1440));

    // What the client is told.
    let screen = shared.screen().unwrap();
    let (current, list) = monitor_list(&screen).unwrap();
    assert_eq!(current, 0);
    assert_eq!(
        list.iter().map(|m| (m.name, m.width)).collect::<Vec<_>>(),
        [("DP-2", 2560), ("HDMI-A-1", 1920)]
    );

    let changes = shared.screen_changes.load(Ordering::Acquire);
    switch_monitor(&mut source, &shared, &mut cursor, 1);
    assert_eq!(shared.screen().unwrap().connector, "HDMI-A-1");
    assert!(shared.keyframe_requested.load(Ordering::Relaxed));
    assert_eq!(cursor.screen, (1920, 1080));
    assert_eq!(shared.screen_changes.load(Ordering::Acquire), changes + 1);
    assert_eq!(monitor_list(&shared.screen().unwrap()).unwrap().0, 1);

    // The same monitor again, or one that is not there: nothing happens.
    shared.keyframe_requested.store(false, Ordering::Relaxed);
    switch_monitor(&mut source, &shared, &mut cursor, 1);
    switch_monitor(&mut source, &shared, &mut cursor, 5);
    assert!(!shared.keyframe_requested.load(Ordering::Relaxed));
    assert_eq!(shared.screen_changes.load(Ordering::Acquire), changes + 1);
}

#[test]
fn long_names_are_clipped_and_lists_capped() {
    let names: Vec<String> = (0..10)
        .map(|i| format!("VERY-LONG-CONNECTOR-{i}"))
        .collect();
    let screen = fernsicht_capture::ScreenInfo {
        connector: names[2].clone(),
        active: names.clone(),
        sizes: vec![(800, 600); 10],
    };
    let (current, list) = monitor_list(&screen).unwrap();
    assert_eq!((current, list.len()), (2, fernsicht_proto::MAX_MONITORS));
    assert!(
        list.iter()
            .all(|m| m.name.len() <= fernsicht_proto::MAX_MONITOR_NAME)
    );
    // The shown monitor beyond the cap: no list rather than a wrong one.
    let far = fernsicht_capture::ScreenInfo {
        connector: names[9].clone(),
        ..screen
    };
    assert!(monitor_list(&far).is_none());
}

fn params(codecs: CodecSet) -> SessionParams {
    SessionParams {
        session_id: 1,
        width: 640,
        height: 360,
        fps: 30,
        bitrate_kbps: 1_000,
        codecs,
        refresh: false,
    }
}

#[test]
fn encoder_falls_back_and_says_what_it_tried() {
    let gpu = EncoderKind::Vaapi {
        render_node: "/dev/dri/renderD-does-not-exist".into(),
    };
    let all = params(CodecSet::of(&[Codec::H264, Codec::Hevc, Codec::Av1]));
    let err = make_encoder(&gpu, &all).err().unwrap().to_string();
    assert!(
        err.contains("Av1") && err.contains("Hevc") && err.contains("H264"),
        "{err}"
    );
    let nothing = make_encoder(&gpu, &params(CodecSet::EMPTY)).err().unwrap();
    assert!(nothing.to_string().contains("none of"), "{nothing}");
    // The synthetic encoder does not care what the client decodes.
    let synthetic = make_encoder(&EncoderKind::Synthetic, &params(CodecSet::EMPTY)).unwrap();
    assert_eq!(synthetic.codec(), Codec::Synthetic);
}

/// An encoder on a GPU that stopped responding.
struct LostGpu;

impl Encoder for LostGpu {
    fn codec(&self) -> Codec {
        Codec::H264
    }

    fn encode(&mut self, _: &Frame, _: &mut EncodedFrame) -> Result<(), CodecError> {
        Err(CodecError::GpuLost)
    }

    fn request_keyframe(&mut self) {}

    fn set_bitrate(&mut self, _: u32) -> Result<(), CodecError> {
        Ok(())
    }
}

#[test]
fn a_gpu_that_stops_responding_ends_the_encode_loop() {
    let shared = Arc::new(shared());
    let slot = Arc::new(Slot::new());
    let (send_tx, send_rx) = bounded::<EncodedFrame>(16);
    let (free_frames_tx, _free_frames) = bounded(16);
    let (free_enc_tx, free_enc_rx) = bounded(16);
    free_enc_tx.send(EncodedFrame::default()).unwrap();
    let rebuild: Rebuild = Box::new(|_, _| Ok(Box::new(LostGpu) as Box<dyn Encoder>));
    let (done_tx, done_rx) = bounded(1);
    let encoder = {
        let (shared, slot) = (shared.clone(), slot.clone());
        std::thread::spawn(move || {
            encode_loop(
                Box::new(LostGpu),
                rebuild,
                2000,
                ((1920, 1080), (1920, 1080)),
                &shared,
                &slot,
                &send_tx,
                &free_frames_tx,
                &free_enc_rx,
                &free_enc_tx,
            );
            done_tx.send(()).unwrap();
        })
    };
    slot.put(Frame::new(1920, 1080, fernsicht_capture::PixelFormat::Nv12));
    // The loop ends by itself (its guard then ends the session), with the
    // slot still open: it does not wait for the next picture.
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the encode loop goes on without a GPU");
    encoder.join().unwrap();
    assert!(send_rx.try_recv().is_err(), "nothing was encoded");
}

#[test]
fn the_stream_takes_the_shape_of_the_monitor_shown() {
    let shared = Arc::new(shared());
    let slot = Arc::new(Slot::new());
    let (send_tx, send_rx) = bounded(16);
    let (free_frames_tx, _free_frames) = bounded(16);
    let (free_enc_tx, free_enc_rx) = bounded(16);
    for _ in 0..8 {
        free_enc_tx.send(EncodedFrame::default()).unwrap();
    }
    let rebuilt = Arc::new(Mutex::new(Vec::new()));
    let rebuild: Rebuild = {
        let rebuilt = rebuilt.clone();
        Box::new(move |kbps, size| {
            rebuilt.lock().unwrap().push(size);
            Ok(Box::new(SyntheticEncoder::new(kbps, 60)) as Box<dyn Encoder>)
        })
    };
    let encoder = {
        let (shared, slot) = (shared.clone(), slot.clone());
        std::thread::spawn(move || {
            encode_loop(
                Box::new(SyntheticEncoder::new(2000, 60)),
                rebuild,
                2000,
                ((1920, 1080), (1920, 1080)),
                &shared,
                &slot,
                &send_tx,
                &free_frames_tx,
                &free_enc_rx,
                &free_enc_tx,
            );
        })
    };
    // A 16:9 monitor, a 21:9 one, and back.
    let mut sizes = Vec::new();
    for (w, h) in [(2560, 1440), (3440, 1440), (3440, 1440), (2560, 1440)] {
        slot.put(Frame::new(w, h, fernsicht_capture::PixelFormat::Nv12));
        let out: EncodedFrame = send_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        sizes.push((out.width, out.height));
    }
    slot.close();
    encoder.join().unwrap();
    assert_eq!(
        sizes,
        [(1920, 1080), (1920, 804), (1920, 804), (1920, 1080)]
    );
    // Rebuilt only when the shape changed (each with a keyframe).
    assert_eq!(*rebuilt.lock().unwrap(), [(1920, 804), (1920, 1080)]);
}

#[test]
fn the_gpu_that_shows_the_monitor_comes_first() {
    use std::os::unix::fs::symlink;
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let sys = std::env::temp_dir().join(format!("fernsicht-gpus-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sys);
    let drm = sys.join("class/drm");
    // A laptop: Intel iGPU (renderD128) drives no monitor here; the AMD
    // dGPU (renderD129) drives DP-1.
    for (pci, vendor, card, render, monitor) in [
        (
            "0000:00:02.0",
            "0x8086",
            "card0",
            "renderD128",
            ("eDP-1", "disabled"),
        ),
        (
            "0000:03:00.0",
            "0x1002",
            "card1",
            "renderD129",
            ("DP-1", "enabled"),
        ),
    ] {
        let dev = sys.join("devices/pci").join(pci);
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("vendor"), vendor).unwrap();
        for node in [card, render] {
            std::fs::create_dir_all(drm.join(node)).unwrap();
            symlink(&dev, drm.join(node).join("device")).unwrap();
        }
        let connector = drm.join(format!("{card}-{}", monitor.0));
        std::fs::create_dir_all(&connector).unwrap();
        std::fs::write(connector.join("enabled"), monitor.1).unwrap();
    }
    let gpus = render_nodes(&sys);
    assert_eq!(
        gpus,
        [
            ("/dev/dri/renderD129".to_owned(), 0x1002),
            ("/dev/dri/renderD128".to_owned(), 0x8086)
        ]
    );
    std::fs::remove_dir_all(&sys).unwrap();
}

#[test]
fn handshake_times_outlive_a_restart_and_reset_on_pairing() {
    let dir = std::env::temp_dir().join(format!("fernsicht-hs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let open = || {
        HostSecurity::new(
            Identity::generate(),
            "zentrale",
            Trusted::default(),
            Some(dir.join("clients.json")),
        )
    };
    let client = Identity::generate().public;
    let sec = open();
    assert!(sec.handshake_is_new(&client, 1000));
    sec.note_handshake(client, 1000);
    assert!(
        !sec.handshake_is_new(&client, 1000),
        "the same again: a replay"
    );
    // After a restart the replay is still recognized.
    let sec = open();
    assert!(!sec.handshake_is_new(&client, 999));
    assert!(sec.handshake_is_new(&client, 1001));
    // Paired anew (its clock may have been set back): forgotten.
    sec.add(Peer {
        key: client,
        name: "bazzite".into(),
        address: None,
        paired_at: 0,
    });
    assert!(open().handshake_is_new(&client, 5));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_frame_rate_is_at_most_the_monitors() {
    assert_eq!(frame_rate(144, Some(60)), 60);
    assert_eq!(frame_rate(60, Some(165)), 60);
    assert_eq!(frame_rate(120, Some(120)), 120);
    // A source without a refresh rate (the test pattern) delivers any.
    assert_eq!(frame_rate(144, None), 144);
    assert_eq!(frame_rate(60, Some(0)), 60);
}

#[test]
fn native_size_is_fitted_into_the_limits() {
    assert_eq!(fit((2560, 1440), (3840, 2160)), (2560, 1440));
    assert_eq!(fit((2560, 1440), (1920, 1080)), (1920, 1080));
    // Aspect ratio kept, even sizes.
    assert_eq!(fit((2560, 1600), (1920, 1080)), (1728, 1080));
    assert_eq!(fit((1366, 768), (3840, 2160)), (1366, 768));
    assert_eq!(fit((1365, 767), (3840, 2160)), (1364, 766));
    assert_eq!(fit((0, 0), (640, 480)), (2, 2));
}

#[test]
fn the_encoder_follows_the_gpu() {
    let amd = ("/dev/dri/renderD128".to_owned(), 0x1002);
    let nv = ("/dev/dri/renderD129".to_owned(), NVIDIA);
    let vaapi = |n: &str| {
        Some(EncoderKind::Vaapi {
            render_node: n.into(),
        })
    };
    assert_eq!(
        choose_encoder(std::slice::from_ref(&amd), true, true),
        vaapi("/dev/dri/renderD128")
    );
    assert_eq!(
        choose_encoder(std::slice::from_ref(&nv), true, true),
        Some(EncoderKind::Nvenc { gpu: 0 })
    );
    // The first GPU the build can use.
    assert_eq!(
        choose_encoder(&[nv.clone(), amd.clone()], true, false),
        vaapi("/dev/dri/renderD128")
    );
    assert_eq!(choose_encoder(&[amd], false, true), None);
    assert_eq!(choose_encoder(&[], true, true), None);
    // This machine: whatever GPUs it has, a build without hardware
    // encoders finds none to use.
    let here = auto_encoder();
    if !cfg!(any(feature = "vaapi", feature = "nvidia")) {
        assert!(here.unwrap_err().to_string().contains("--encoder"));
    }
}

#[test]
fn the_host_describes_itself() {
    let test = HostDescription::of_this_machine(&EncoderKind::Synthetic);
    assert_eq!(test.gpu, "Testbild");
    assert!(!test.os.is_empty());
    // No such GPU: nothing claimed.
    let unknown = HostDescription::of_this_machine(&EncoderKind::Vaapi {
        render_node: "/dev/dri/renderD999".into(),
    });
    assert_eq!(unknown.gpu, "");
    assert_eq!(
        describe_gpu(Some("Radeon RX 7800 XT".into()), Some(Codec::Av1)),
        "Radeon RX 7800 XT · AV1"
    );
    assert_eq!(describe_gpu(Some("GTX 1080".into()), None), "GTX 1080");
    assert_eq!(describe_gpu(None, Some(Codec::Hevc)), "HEVC");
}

#[test]
fn default_bitrate_follows_the_pixel_rate() {
    assert_eq!(default_bitrate_kbps(1920, 1080, 60), 20_000);
    assert_eq!(default_bitrate_kbps(2560, 1440, 60), 35_556);
    assert_eq!(default_bitrate_kbps(1920, 1080, 30), 10_000);
    assert_eq!(default_bitrate_kbps(320, 240, 30), 1_000, "floor");
    assert_eq!(default_bitrate_kbps(3840, 2160, 144), 192_000);
}

#[test]
fn repeated_errors_are_collapsed() {
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let mut e = RepeatedError::default();
    assert_eq!(e.report("bad".into(), t0).as_deref(), Some("bad"));
    for i in 1..60 {
        assert_eq!(e.report("bad".into(), t0 + ms(i * 16)), None);
    }
    assert_eq!(
        e.report("bad".into(), t0 + ms(1000)).as_deref(),
        Some("bad (another 60× in the last second)")
    );
    assert_eq!(e.report("bad".into(), t0 + ms(1016)), None);
    // A different error is reported at once.
    assert_eq!(
        e.report("worse".into(), t0 + ms(1020)).as_deref(),
        Some("worse")
    );
}

#[test]
fn the_pointer_hotspot_is_learnt_from_where_input_put_it() {
    use crate::pointer::hotspot;
    use fernsicht_capture::{CursorImage, CursorState};
    use fernsicht_proto::Packet;

    // The middle of a 2560×1440 screen is pixel (1280, 720); an image at
    // (1270, 715) has the pointer 10 right and 5 down in it.
    let middle = (32_768, 32_768);
    assert_eq!(
        hotspot(middle, (2560, 1440), (1270, 715), (32, 32)),
        Some((10, 5))
    );
    assert_eq!(
        hotspot(middle, (2560, 1440), (1300, 715), (32, 32)),
        None,
        "left of it"
    );
    assert_eq!(
        hotspot(middle, (2560, 1440), (1240, 715), (32, 32)),
        None,
        "right of it"
    );

    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let link = Link::Udp(UdpLink {
        peer: socket.local_addr().unwrap(),
        socket: socket.clone(),
        session_id: 1,
        crypto: None,
        confirmed: Arc::new(AtomicBool::new(true)),
        gso: Arc::new(AtomicBool::new(true)),
    });
    let mut cursor = CursorSender::new(link, 1, (2560, 1440));
    let state = CursorState {
        visible: true,
        x: 1270,
        y: 715,
        serial: 3,
        image: Arc::new(CursorImage {
            width: 32,
            height: 32,
            pixels: vec![0; 32 * 32 * 4],
        }),
    };
    // The hotspot in the last position packet sent.
    let sent = |cursor: &mut CursorSender, pointer| {
        cursor.send(&state, false, pointer);
        let mut buf = [0u8; 2048];
        let mut last = None;
        while let Ok(n) = socket.recv(&mut buf) {
            if let Ok(Packet::Cursor(c)) = Packet::decode(&buf[..n]) {
                last = Some(c.hotspot);
            }
        }
        last.expect("a position")
    };
    // Just moved: the compositor may not have drawn it there yet.
    assert_eq!(sent(&mut cursor, Some((middle, 10))), None);
    assert_eq!(sent(&mut cursor, Some((middle, 100))), Some((10, 5)));
    // Kept for the image, also once the input is old.
    assert_eq!(sent(&mut cursor, Some((middle, 60_000))), Some((10, 5)));
    assert_eq!(sent(&mut cursor, None), Some((10, 5)));
}

#[test]
fn intra_refresh_is_agreed_only_with_nvenc() {
    use crate::HostAgent;
    use crate::config::HostConfig;
    use fernsicht_proto::Hello;
    let hello = Hello {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_kbps: 0,
        codecs: CodecSet::H264,
        refresh: true,
    };
    let agent = |encoder| {
        HostAgent::bind(HostConfig {
            bind: "127.0.0.1:0".into(),
            encoder,
            ..HostConfig::default()
        })
        .unwrap()
    };
    let nvenc = agent(EncoderKind::Nvenc { gpu: 0 });
    let p = nvenc.negotiate(&hello);
    assert!(p.refresh);
    assert!(
        nvenc.hello_ack(&p, Codec::H264).refresh,
        "the client is told"
    );
    // VAAPI and the synthetic encoder keep keyframes.
    let vaapi = agent(EncoderKind::Vaapi {
        render_node: "/dev/dri/renderD128".into(),
    });
    assert!(!vaapi.negotiate(&hello).refresh);
    assert!(!agent(EncoderKind::Synthetic).negotiate(&hello).refresh);
    // Not asked for: not used.
    let plain = Hello {
        refresh: false,
        ..hello
    };
    assert!(!nvenc.negotiate(&plain).refresh);
}
