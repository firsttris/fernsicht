//! Session lifecycle across real host and client.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use fernsicht_client::{AudioOutput, ClientConfig, InputEvent, InputHandle, run};
use fernsicht_e2e::{Host, ImpairedLink, Impairment};
use fernsicht_host_agent::{AudioKind, HostConfig, InputKind};
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

fn opus() -> bool {
    match fernsicht_audio::opus::available() {
        Ok(()) => true,
        Err(e) if std::env::var_os("FERNSICHT_REQUIRE_OPUS").is_none() => {
            eprintln!("skipped: {e}");
            false
        }
        Err(e) => panic!("FERNSICHT_REQUIRE_OPUS is set but: {e}"),
    }
}

/// Plays the host's test tone for `secs` through `down` and returns the
/// summary and what was played.
fn tone(down: Impairment, secs: u64) -> (fernsicht_client::RunSummary, Vec<i16>) {
    let host = Host::start(HostConfig {
        audio: AudioKind::Tone,
        ..HostConfig::default()
    });
    let link = ImpairedLink::start(host.addr(), down, Impairment::none(), 3);
    let recorder = fernsicht_audio::Recorder::default();
    let s = run(
        ClientConfig {
            host: link.addr().to_string(),
            width: 640,
            height: 360,
            duration: Some(Duration::from_secs(secs)),
            audio: AudioOutput::Record(recorder.clone()),
            ..ClientConfig::default()
        },
        Arc::new(AtomicBool::new(false)),
    )
    .expect("client failed");
    drop(link);
    host.stop();
    let pcm = recorder.played();
    eprintln!("{down:?}: {:?}", s.audio);
    (s, pcm)
}

/// Share of the left channel's energy at `hz`, measured in 50 ms blocks
/// (Goertzel per block). One filter over the whole recording would be
/// under 1 Hz wide: a single phase jump where a lost packet was concealed
/// would smear the tone's energy and make a clean tone look broken.
fn share_at(pcm: &[i16], hz: f64) -> f64 {
    // 2400 samples: a whole number of cycles at 440 Hz (22), no leakage.
    const BLOCK: usize = 2400;
    let w = std::f64::consts::TAU * hz / 48_000.0;
    let (mut power, mut total) = (0.0, 0.0);
    for block in pcm.as_chunks::<{ 2 * BLOCK }>().0 {
        let x: Vec<f64> = block.iter().step_by(2).map(|&v| f64::from(v)).collect();
        let (mut s1, mut s2) = (0.0, 0.0);
        for &v in &x {
            let s = v + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        power += s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2;
        total += x.iter().map(|v| v * v).sum::<f64>() * BLOCK as f64 / 2.0;
    }
    power / total.max(1.0)
}

#[test]
fn the_hosts_sound_is_played() {
    if !opus() {
        return;
    }
    let _serial = fernsicht_e2e::exclusive();
    let (s, pcm) = tone(Impairment::none(), 3);
    // 200 frames a second; start-up takes a moment.
    assert!(s.audio.played > 400, "{:?}", s.audio);
    assert!(s.audio.concealed <= 2, "{:?}", s.audio);
    let middle = &pcm[pcm.len() / 4..pcm.len() * 3 / 4];
    let share = share_at(middle, 440.0);
    assert!(
        share > 0.8,
        "440 Hz is {share:.2} of what was played: {:?}",
        s.audio
    );
    // Buffers: 15 ms jitter + 10 ms output + transport; generous bound.
    assert!(s.audio.delay_us < 80_000, "{:?}", s.audio);
}

#[test]
fn lost_sound_packets_are_mostly_repaired() {
    if !opus() {
        return;
    }
    let _serial = fernsicht_e2e::exclusive();
    let (s, pcm) = tone(Impairment::loss(0.2), 3);
    assert!(s.audio.played > 400, "{:?}", s.audio);
    // Each packet repeats the previous frame: only two losses in a row
    // (4 % of the time at 20 % loss) need concealment.
    let concealed = s.audio.concealed as f64 / (s.audio.played + s.audio.concealed) as f64;
    assert!(concealed < 0.1, "{concealed:.3} concealed: {:?}", s.audio);
    let middle = &pcm[pcm.len() / 4..pcm.len() * 3 / 4];
    let share = share_at(middle, 440.0);
    assert!(
        share > 0.9,
        "440 Hz is {share:.2} of what was played: {:?}",
        s.audio
    );
}
