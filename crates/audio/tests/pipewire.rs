//! Playback through a native PipeWire stream, on the machine's real sound
//! server. It plays silence, so it runs only when asked for:
//! `FERNSICHT_SOUND=1` (no sound server in CI either).

use std::time::{Duration, Instant};

use fernsicht_audio::pipewire::Playback;
use fernsicht_audio::{AudioSink, CHANNELS, FRAME_SAMPLES};

#[test]
fn a_native_stream_keeps_little_buffered_and_never_runs_dry() {
    if std::env::var_os("FERNSICHT_SOUND").is_none() {
        eprintln!("skipped: set FERNSICHT_SOUND=1 (plays silence on this machine)");
        return;
    }
    let mut out = Playback::open(1).expect("a PipeWire stream");
    let silence = [0i16; FRAME_SAMPLES * CHANNELS];
    let start = Instant::now();
    let mut buffered = Vec::new();
    while start.elapsed() < Duration::from_secs(3) {
        out.play(&silence).unwrap();
        buffered.push(out.buffered_us());
    }
    buffered.sort_unstable();
    let median = buffered[buffered.len() / 2];
    let (quantum, starved) = out.graph_stats();
    println!("median {median} µs after the client, quantum {quantum}, {starved} frames silent");
    // pipewire-pulse held about 21 ms here.
    assert!(median < 15_000, "{median} µs buffered");
    assert!(quantum > 0, "the graph never asked for sound");
    assert_eq!(starved, 0, "the stream ran dry");
}
