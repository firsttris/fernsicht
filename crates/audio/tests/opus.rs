//! Opus round trips. Needs libopus at runtime; skips without it unless
//! `FERNSICHT_REQUIRE_OPUS=1` (set in CI, so the test cannot skip there).

use fernsicht_audio::opus::{Decoder, Encoder, MAX_PACKET};
use fernsicht_audio::{AudioSource, CHANNELS, FRAME_SAMPLES, Frame, Tone};

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

/// Energy of `pcm` at `hz` (Goertzel), relative to the total energy.
fn tone_share(pcm: &[i16], hz: f64) -> f64 {
    let left: Vec<f64> = pcm
        .iter()
        .step_by(CHANNELS)
        .map(|&v| f64::from(v))
        .collect();
    let n = left.len() as f64;
    let w = std::f64::consts::TAU * hz / 48_000.0;
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in &left {
        let s = x + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2;
    let total: f64 = left.iter().map(|x| x * x).sum::<f64>() * n / 2.0;
    power / total.max(1.0)
}

#[test]
fn a_tone_survives_encoding() {
    if !opus() {
        return;
    }
    let mut src = Tone::new(1000.0);
    let mut enc = Encoder::new(128_000).unwrap();
    let mut dec = Decoder::new().unwrap();
    let mut pcm: Frame = [0; FRAME_SAMPLES * CHANNELS];
    let mut out = pcm;
    let mut packet = [0u8; MAX_PACKET];
    let mut heard = Vec::new();
    let mut sizes = Vec::new();
    for _ in 0..40 {
        src.next_frame(&mut pcm).unwrap();
        let n = enc.encode(&pcm, &mut packet).unwrap();
        sizes.push(n);
        dec.decode(Some(&packet[..n]), &mut out).unwrap();
        heard.extend_from_slice(&out);
    }
    // 128 kbit/s at 200 frames/s: 80 bytes each.
    assert!(
        sizes.iter().all(|&n| (40..=MAX_PACKET).contains(&n)),
        "{sizes:?}"
    );
    // After the codec's start-up, the 1 kHz tone is most of what is heard.
    let share = tone_share(&heard[heard.len() / 2..], 1000.0);
    assert!(share > 0.8, "1 kHz is {share:.2} of the energy");
}

#[test]
fn a_lost_frame_is_concealed_not_silenced() {
    if !opus() {
        return;
    }
    let mut src = Tone::new(500.0);
    let mut enc = Encoder::new(128_000).unwrap();
    let mut dec = Decoder::new().unwrap();
    let mut pcm: Frame = [0; FRAME_SAMPLES * CHANNELS];
    let mut out = pcm;
    let mut packet = [0u8; MAX_PACKET];
    for _ in 0..20 {
        src.next_frame(&mut pcm).unwrap();
        let n = enc.encode(&pcm, &mut packet).unwrap();
        dec.decode(Some(&packet[..n]), &mut out).unwrap();
    }
    dec.decode(None, &mut out).unwrap();
    let energy: f64 = out.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / out.len() as f64;
    assert!(energy.sqrt() > 1000.0, "concealment keeps the sound going");
    // Garbage is an error or concealed, never a crash.
    let _ = dec.decode(Some(&[0xff; 3]), &mut out);
}
