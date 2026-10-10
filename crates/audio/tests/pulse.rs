//! Capture and playback against a real sound server. Playing makes sound,
//! so this only runs with `FERNSICHT_PULSE_TEST=1` (set in CI, where the
//! server has a null output): never on a developer's speakers by accident.
//!
//! The test plays a tone and records the output's monitor: the same tone
//! must come back.

use std::time::Duration;

use fernsicht_audio::pulse::{Capture, Playback};
use fernsicht_audio::{AudioSink, AudioSource, CHANNELS, FRAME_SAMPLES, Frame, Tone};

fn enabled() -> bool {
    let on = std::env::var_os("FERNSICHT_PULSE_TEST").is_some();
    if !on {
        eprintln!("skipped: set FERNSICHT_PULSE_TEST=1 (plays a tone)");
    }
    on
}

/// Share of the left channel's energy at `hz` (Goertzel).
fn share_at(pcm: &[i16], hz: f64) -> f64 {
    let x: Vec<f64> = pcm
        .iter()
        .step_by(CHANNELS)
        .map(|&v| f64::from(v))
        .collect();
    let w = std::f64::consts::TAU * hz / 48_000.0;
    let (mut s1, mut s2) = (0.0, 0.0);
    for &v in &x {
        let s = v + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2;
    let total: f64 = x.iter().map(|v| v * v).sum::<f64>() * x.len() as f64 / 2.0;
    power / total.max(1.0)
}

#[test]
fn what_is_played_can_be_captured() {
    if !enabled() {
        return;
    }
    let mut capture = Capture::open().expect("capture");
    let player = std::thread::spawn(|| {
        let mut out = Playback::open(2).expect("playback");
        let mut tone = Tone::new(880.0);
        let mut pcm: Frame = [0; FRAME_SAMPLES * CHANNELS];
        let mut buffered = 0;
        for _ in 0..300 {
            tone.next_frame(&mut pcm).unwrap();
            out.play(&pcm).unwrap();
            buffered = buffered.max(out.buffered_us());
        }
        buffered
    });
    std::thread::sleep(Duration::from_millis(300));
    let mut heard = Vec::new();
    let mut pcm: Frame = [0; FRAME_SAMPLES * CHANNELS];
    for _ in 0..100 {
        let t = capture.next_frame(&mut pcm).expect("record");
        assert!(t <= fernsicht_core::now_us());
        heard.extend_from_slice(&pcm);
    }
    let buffered = player.join().unwrap();
    assert!(
        buffered > 0 && buffered < 200_000,
        "output buffer {buffered} µs"
    );
    let share = share_at(&heard, 880.0);
    assert!(share > 0.5, "880 Hz is {share:.2} of the monitor");
}
