//! Session lifecycle across real host and client.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use fernsicht_client::{ClientConfig, InputEvent, InputHandle, run};
use fernsicht_e2e::{Host, ImpairedLink, Impairment};
use fernsicht_host_agent::{HostConfig, InputKind};
use fernsicht_input::Recorder;

fn client(host: &Host, secs: f32) -> fernsicht_client::RunSummary {
    run(
        ClientConfig {
            host: host.addr().to_string(),
            width: 640,
            height: 360,
            duration: Some(Duration::from_secs_f32(secs)),
            ..ClientConfig::default()
        },
        Arc::new(AtomicBool::new(false)),
    )
    .expect("client failed")
}

#[test]
fn sequential_clients_get_separate_sessions() {
    let _serial = fernsicht_e2e::exclusive();
    let host = Host::start(HostConfig::default());
    let a = client(&host, 1.0);
    let b = client(&host, 1.0);
    assert!(a.frames_presented > 30 && b.frames_presented > 30);
    assert_ne!(a.session_id, b.session_id);
    // Both sessions start with their own keyframe.
    assert!(a.keyframes_decoded >= 1 && b.keyframes_decoded >= 1);
}

#[test]
fn host_shutdown_ends_the_client_cleanly() {
    let _serial = fernsicht_e2e::exclusive();
    let host = Host::start(HostConfig::default());
    let addr = host.addr().to_string();
    let c = std::thread::spawn(move || {
        run(
            ClientConfig {
                host: addr,
                width: 640,
                height: 360,
                duration: Some(Duration::from_secs(20)),
                ..ClientConfig::default()
            },
            Arc::new(AtomicBool::new(false)),
        )
    });
    std::thread::sleep(Duration::from_millis(800));
    let stopped = Instant::now();
    host.stop();
    let s = c.join().unwrap().expect("client should end with Ok on Bye");
    assert!(stopped.elapsed() < Duration::from_secs(3));
    assert!(s.frames_presented > 20, "{s:?}");
}

#[test]
fn client_requested_settings_are_honoured() {
    let _serial = fernsicht_e2e::exclusive();
    let host = Host::start(HostConfig::default());
    let s = run(
        ClientConfig {
            host: host.addr().to_string(),
            width: 800,
            height: 600,
            fps: 30,
            bitrate_kbps: 2_000,
            duration: Some(Duration::from_secs(2)),
            ..ClientConfig::default()
        },
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    // ~30 fps, not 60.
    assert!((40..=75).contains(&s.frames_presented), "{s:?}");
}

#[test]
fn the_pointer_reaches_the_client() {
    let _serial = fernsicht_e2e::exclusive();
    let host = Host::start(HostConfig::default());
    let s = client(&host, 1.5);
    // The test pattern's arrow: one image, a position with every frame.
    assert_eq!(s.cursor_shapes, 1, "{s:?}");
    assert!(s.cursor_positions > 60, "{s:?}");
}

#[test]
fn the_pointer_image_heals_after_loss() {
    let _serial = fernsicht_e2e::exclusive();
    let host = Host::start(HostConfig::default());
    // Half of all packets to the client lost, the image's included: the
    // repeat every 2 s brings it eventually.
    let link = ImpairedLink::start(host.addr(), Impairment::loss(0.5), Impairment::none(), 11);
    let s = run(
        ClientConfig {
            host: link.addr().to_string(),
            width: 640,
            height: 360,
            duration: Some(Duration::from_secs(5)),
            ..ClientConfig::default()
        },
        Arc::new(AtomicBool::new(false)),
    )
    .expect("client failed");
    assert!(s.cursor_shapes >= 1, "{s:?}");
    assert!(s.cursor_positions > 60, "{s:?}");
}

#[test]
fn keys_arrive_once_and_in_order_through_loss() {
    let _serial = fernsicht_e2e::exclusive();
    let recorder = Recorder::default();
    let host = Host::start(HostConfig {
        input: InputKind::Record(recorder.clone()),
        ..HostConfig::default()
    });
    // 30 % of the packets lost both ways: input and acks.
    let link = ImpairedLink::start(host.addr(), Impairment::loss(0.3), Impairment::loss(0.3), 5);
    let input = std::sync::Arc::new(InputHandle::default());
    let typed: Vec<InputEvent> = (0..40u16)
        .map(|i| InputEvent::Key {
            code: 16 + i / 2,
            pressed: i % 2 == 0,
        })
        .collect();
    let feeder = {
        let (input, typed) = (input.clone(), typed.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            for e in typed {
                input.push(e);
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    run(
        ClientConfig {
            host: link.addr().to_string(),
            width: 640,
            height: 360,
            duration: Some(Duration::from_secs(4)),
            input: Some(input),
            ..ClientConfig::default()
        },
        Arc::new(AtomicBool::new(false)),
    )
    .expect("client failed");
    feeder.join().unwrap();
    drop(link);
    host.stop();
    assert_eq!(*recorder.0.lock().unwrap(), typed);
}
