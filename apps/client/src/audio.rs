//! The sound thread: Opus packets through the jitter buffer to the output.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::Receiver;
use fernsicht_audio::jitter::Encoded;
use fernsicht_core::now_us;

use crate::config::AudioOutput;
use crate::summary::AudioSummary;

/// One audio packet for the playback thread.
pub(crate) struct AudioPacket {
    pub(crate) seq: u32,
    pub(crate) frame: Encoded,
    /// Empty for the first packet.
    pub(crate) previous: Encoded,
    /// When the host played it, on our clock (µs).
    pub(crate) captured_local: i64,
}

/// Frames the jitter buffer holds before playing (15 ms).
const AUDIO_JITTER_FRAMES: usize = 3;

/// Frames kept ahead of a native PipeWire stream (5 ms; the graph then
/// holds about a quantum more: 8 ms in all, measured, with no gap even
/// with every core busy).
const PIPEWIRE_AHEAD_FRAMES: usize = 1;

/// What the PulseAudio path orders from the server (10 ms; pipewire-pulse
/// holds about 21 ms whatever it is asked).
const PULSE_OUTPUT_FRAMES: u32 = 2;

fn audio_sink(out: &AudioOutput) -> Option<Box<dyn fernsicht_audio::AudioSink>> {
    match out {
        AudioOutput::Off => None,
        AudioOutput::Record(r) => Some(Box::new(r.clone())),
        AudioOutput::Speakers => speakers(),
    }
}

/// PipeWire's own stream where it can be had (about 13 ms less delay),
/// else PulseAudio's API (pipewire-pulse, or a real PulseAudio).
fn speakers() -> Option<Box<dyn fernsicht_audio::AudioSink>> {
    let pipewire = match fernsicht_audio::pipewire::Playback::open(PIPEWIRE_AHEAD_FRAMES) {
        Ok(p) => {
            log::info!("sound through PipeWire");
            return Some(Box::new(p));
        }
        Err(e) => e,
    };
    match fernsicht_audio::pulse::Playback::open(PULSE_OUTPUT_FRAMES) {
        Ok(p) => {
            log::info!("sound through PulseAudio ({pipewire})");
            Some(Box::new(p))
        }
        Err(e) => {
            log::warn!("no sound: {e}; {pipewire}");
            None
        }
    }
}

/// Plays the host's sound: jitter buffer, Opus decoding with concealment
/// of lost frames, and a sink whose blocking writes pace the loop. The
/// output opens with the first sound from the host. Ends when the network
/// thread hangs up.
pub(crate) fn audio_loop(
    rx: &Receiver<AudioPacket>,
    out: &AudioOutput,
    muted: &AtomicBool,
) -> AudioSummary {
    use fernsicht_audio::jitter::{Jitter, Next};
    let Ok(first) = rx.recv() else {
        return AudioSummary::default();
    };
    let Some(mut sink) = audio_sink(out) else {
        // Drain, so the network thread never blocks on us.
        while rx.recv().is_ok() {}
        return AudioSummary::default();
    };
    let mut decoder = match fernsicht_audio::opus::Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("no sound: {e}");
            return AudioSummary::default();
        }
    };
    let mut jitter = Jitter::new(AUDIO_JITTER_FRAMES);
    let frame_us = u64::from(fernsicht_audio::FRAME_MS) * 1000;
    // The host's sound card and ours drift apart: frames a sample shorter
    // or longer keep the delay at the jitter target.
    let mut drift =
        fernsicht_audio::drift::DriftControl::new(AUDIO_JITTER_FRAMES as u64 * frame_us);
    let mut pcm: fernsicht_audio::Frame =
        [0; fernsicht_audio::FRAME_SAMPLES * fernsicht_audio::CHANNELS];
    let mut stretched = Vec::with_capacity(pcm.len() + fernsicht_audio::CHANNELS);
    // The newest frame's seq and when the host played it, on our clock.
    let mut captured: Option<(u32, i64)> = None;
    let mut started = false;
    let mut delays = (0u64, 0u64);
    take(first, &mut jitter, &mut captured);
    fn take(p: AudioPacket, jitter: &mut Jitter, captured: &mut Option<(u32, i64)>) {
        jitter.push(p.seq, p.frame.as_slice());
        jitter.push(p.seq.wrapping_sub(1), p.previous.as_slice());
        if captured.is_none_or(|(seq, _)| (p.seq.wrapping_sub(seq) as i32) > 0) {
            *captured = Some((p.seq, p.captured_local));
        }
    }
    loop {
        // Wait for sound before starting; once playing, the sink paces us.
        if !started {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(p) => take(p, &mut jitter, &mut captured),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        loop {
            match rx.try_recv() {
                Ok(p) => take(p, &mut jitter, &mut captured),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    return summary_of(&jitter, delays);
                }
            }
        }
        let result = match jitter.pop() {
            Next::Frame(data) => decoder.decode(Some(data), &mut pcm),
            Next::Lost => decoder.decode(None, &mut pcm),
            Next::Empty if started => decoder.decode(None, &mut pcm),
            Next::Empty => continue,
        };
        if let Err(e) = result {
            log::debug!("audio decode: {e}");
            pcm.fill(0);
        }
        if muted.load(Ordering::Relaxed) {
            pcm.fill(0);
        }
        started = true;
        let delta = drift.step(jitter.depth() as u64 * frame_us);
        fernsicht_audio::drift::stretch(&pcm, fernsicht_audio::CHANNELS, delta, &mut stretched);
        if let Err(e) = sink.play(&stretched) {
            log::warn!("sound output: {e}");
            break;
        }
        // Delay of the newest frame: from the host's output to ours.
        if let Some((_, t)) = captured {
            let queued = jitter.depth() as u64 * frame_us;
            let d = ((now_us() as i64 - t).max(0) as u64)
                .saturating_add(queued)
                .saturating_add(sink.buffered_us());
            delays = (delays.0.saturating_add(d), delays.1 + 1);
        }
    }
    summary_of(&jitter, delays)
}

/// What the jitter buffer counted, and the average of the `delays`
/// (sum, count).
fn summary_of(jitter: &fernsicht_audio::jitter::Jitter, (sum, n): (u64, u64)) -> AudioSummary {
    AudioSummary {
        played: jitter.played,
        concealed: jitter.concealed,
        dropped: jitter.dropped,
        delay_us: sum.checked_div(n).unwrap_or(0),
    }
}
