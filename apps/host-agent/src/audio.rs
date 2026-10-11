//! The sound thread: the host's output as Opus, two frames per packet.

use std::sync::atomic::Ordering;

use fernsicht_audio::{AudioSource, Frame as AudioFrame, Tone};
use fernsicht_proto::{AudioHeader, MAX_AUDIO_FRAME, MAX_DATAGRAM};

use crate::config::AudioKind;
use crate::link::Link;
use crate::shared::{HostStats, Shared};

/// Opus bitrate for the desktop's sound: transparent for music.
const AUDIO_BITRATE: u32 = 128_000;

pub(crate) fn make_audio(kind: AudioKind) -> Result<Option<Box<dyn AudioSource>>, String> {
    match kind {
        AudioKind::Off => Ok(None),
        AudioKind::Tone => Ok(Some(Box::new(Tone::new(440.0)))),
        AudioKind::Desktop => Ok(Some(Box::new(fernsicht_audio::pulse::Capture::open()?))),
    }
}

/// Captures, encodes and sends 5 ms frames until the session ends. Each
/// packet repeats the previous frame, so one lost packet leaves no gap.
pub(crate) fn audio_loop(
    mut source: Box<dyn AudioSource>,
    shared: &Shared,
    link: &Link,
    session_id: u32,
) -> Result<(), String> {
    let mut encoder = fernsicht_audio::opus::Encoder::new(AUDIO_BITRATE)?;
    let mut pcm: AudioFrame = [0; fernsicht_audio::FRAME_SAMPLES * fernsicht_audio::CHANNELS];
    let mut current = [0u8; MAX_AUDIO_FRAME];
    let mut previous = Vec::with_capacity(MAX_AUDIO_FRAME);
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut seq = 0u32;
    while shared.running.load(Ordering::Acquire) {
        let captured_us = source.next_frame(&mut pcm)?;
        let n = encoder.encode(&pcm, &mut current)?;
        let len = AudioHeader {
            session_id,
            seq,
            capture_us: captured_us,
        }
        .encode(&current[..n], &previous, &mut buf);
        match link {
            // WebRTC carries Opus frames as they are.
            Link::Web(tx) => {
                let _ = tx.try_send(crate::web::WebOut::Audio {
                    data: current[..n].to_vec(),
                });
            }
            Link::Udp(_) => {
                let _ = link.send(&buf[..len]);
            }
        }
        HostStats::bump(&shared.stats.audio_frames_sent);
        previous.clear();
        previous.extend_from_slice(&current[..n]);
        seq = seq.wrapping_add(1);
    }
    Ok(())
}
