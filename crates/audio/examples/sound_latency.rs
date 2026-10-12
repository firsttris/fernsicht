//! How much the sound server buffers on Fernsicht's sound paths: playback
//! as the client opens it, writing silence, through PulseAudio's simple
//! API (served by pipewire-pulse) and through a native PipeWire stream;
//! and capture of the default output's monitor as the host opens it.
//! For D10 (native PipeWire), measured before and after.
//!
//!   cargo run -p fernsicht-audio --example sound_latency -- [seconds] [frames]
//!
//! `frames`: the 5 ms frames the client keeps ahead of the server.

use std::time::{Duration, Instant};

use fernsicht_audio::pulse::Capture;
use fernsicht_audio::{AudioSink, AudioSource, CHANNELS, FRAME_SAMPLES, Frame};

fn spread(mut v: Vec<u64>) -> String {
    if v.is_empty() {
        return "no samples".into();
    }
    v.sort_unstable();
    let at = |p: f64| v[((v.len() as f64 * p) as usize).min(v.len() - 1)] as f64 / 1000.0;
    format!(
        "median {:.1} ms · p10 {:.1} · p90 {:.1} · max {:.1} ({} samples)",
        at(0.5),
        at(0.1),
        at(0.9),
        at(1.0),
        v.len()
    )
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let frames: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let run = Duration::from_secs(secs);

    // As the client.
    let silence = [0i16; FRAME_SAMPLES * CHANNELS];
    let measure = |out: &mut dyn AudioSink| {
        let start = Instant::now();
        let mut buffered = Vec::new();
        while start.elapsed() < run {
            out.play(&silence).expect("play");
            buffered.push(out.buffered_us());
        }
        spread(buffered)
    };
    match fernsicht_audio::pulse::Playback::open(frames) {
        Ok(mut out) => println!(
            "playback (pulse, {frames} frames ahead), buffered after the client: {}",
            measure(&mut out)
        ),
        Err(e) => println!("playback (pulse): {e}"),
    }
    match fernsicht_audio::pipewire::Playback::open(frames as usize) {
        Ok(mut out) => {
            let spread = measure(&mut out);
            let (quantum, starved) = out.graph_stats();
            println!(
                "playback (pipewire, {frames} frames ahead), buffered after the client: {spread}; \
                 graph asks {quantum} frames a cycle, {starved} frames played as silence"
            );
        }
        Err(e) => println!("playback (pipewire): {e}"),
    }

    // As the host: the monitor of the default output, a frame per read.
    let mut cap = Capture::open().expect("capture");
    let mut pcm: Frame = [0; FRAME_SAMPLES * CHANNELS];
    let start = Instant::now();
    let mut ages = Vec::new();
    while start.elapsed() < run {
        let played = cap.next_frame(&mut pcm).expect("record");
        ages.push(fernsicht_core::now_us().saturating_sub(played));
    }
    println!("capture (host), age of the newest sample: {}", spread(ages));
}
