//! Hardware tests for NVENC/NVDEC. They need an NVIDIA GPU and run only
//! when `FERNSICHT_GPU=nvidia` (the GPU CI job on the NVIDIA runner);
//! otherwise only the test without a driver runs. Measurements are
//! appended to the CI job summary.
#![cfg(feature = "nvidia")]

use std::io::Write;
use std::time::Instant;

use fernsicht_capture::{Frame, FrameSource, TestPattern};
use fernsicht_codec::nvidia::{NvdecDecoder, NvencConfig, NvencEncoder};
use fernsicht_codec::{
    CodecError, DecodedFrame, Decoder, EncodedFrame, Encoder, Picture, PictureKind,
};

fn nvidia() -> bool {
    let on = std::env::var("FERNSICHT_GPU").as_deref() == Ok("nvidia");
    if !on {
        eprintln!("skipped: set FERNSICHT_GPU=nvidia (needs an NVIDIA GPU)");
    }
    on
}

fn config(width: u32, height: u32, fps: u32, kbps: u32) -> NvencConfig {
    NvencConfig {
        gpu: 0,
        width,
        height,
        fps,
        bitrate_kbps: kbps,
    }
}

/// Test frames with texture, without the pacing of a real source.
fn frames(width: u32, height: u32, count: usize) -> Vec<Frame> {
    let mut src = TestPattern::new(width, height, 100_000);
    (0..count)
        .map(|_| {
            let mut f = src.alloc_frame();
            src.next_frame(&mut f).unwrap();
            let w = width as usize;
            for (i, px) in f.data[..w * height as usize].iter_mut().enumerate() {
                let (x, y) = (i % w, i / w);
                *px = px.wrapping_add(((x + y + f.seq as usize * 7) % 64) as u8);
            }
            f
        })
        .collect()
}

fn psnr_y(a: &[u8], b: &[u8], luma_len: usize) -> f64 {
    let mse: f64 = a[..luma_len]
        .iter()
        .zip(&b[..luma_len])
        .map(|(&x, &y)| {
            let d = f64::from(x) - f64::from(y);
            d * d
        })
        .sum::<f64>()
        / luma_len as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

fn percentile(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() as f64 * p).ceil() as usize).clamp(1, v.len()) - 1]
}

fn summary(line: &str) {
    println!("{line}");
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY")
        && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

#[test]
fn roundtrip_1080p60_quality_latency_and_framing() {
    if !nvidia() {
        return;
    }
    let (w, h, fps, kbps) = (1920, 1080, 60, 20_000);
    let mut enc = NvencEncoder::new(&config(w, h, fps, kbps)).unwrap();
    let mut dec = NvdecDecoder::new(0).unwrap();
    let input = frames(w, h, 120);
    let (mut out, mut decoded) = (EncodedFrame::default(), DecodedFrame::default());
    let (mut enc_ms, mut dec_ms, mut dl_ms, mut sizes, mut psnrs) =
        (vec![], vec![], vec![], vec![], vec![]);

    for (i, f) in input.iter().enumerate() {
        let t = Instant::now();
        // encode() fails if NVENC held the frame back (a frame of latency).
        enc.encode(f, &mut out).unwrap();
        enc_ms.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(
            out.keyframe,
            i == 0,
            "frame {i}: only the first is a keyframe"
        );
        sizes.push(out.data.len());

        let t = Instant::now();
        dec.decode(&out.data, &mut decoded).unwrap();
        dec_ms.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!((decoded.width, decoded.height), (w, h));
        assert_eq!(decoded.seq, i as u64, "one frame per packet, in order");

        let t = Instant::now();
        let pic = dec.picture(PictureKind::DmaBuf).unwrap().unwrap();
        dl_ms.push(t.elapsed().as_secs_f64() * 1e3);
        let Picture::Nv12 { data, .. } = pic else {
            panic!("NVDEC hands out CPU pictures for now")
        };
        if i % 10 == 0 {
            psnrs.push(psnr_y(&f.data, data, (w * h) as usize));
        }
    }
    assert!(dec.last_frame_is_bt709_limited(), "colour not signalled");

    let min_psnr = psnrs.iter().copied().fold(f64::INFINITY, f64::min);
    let avg_delta = sizes[1..].iter().sum::<usize>() as f64 / (sizes.len() - 1) as f64;
    let target = f64::from(kbps) * 1000.0 / 8.0 / f64::from(fps);
    summary("### NVENC/NVDEC H.264 1080p60, 20 Mbit/s (120 Frames)");
    summary("| Messung | Wert |\n|---|---|");
    for (name, v) in [
        ("Encode inkl. Upload", &enc_ms),
        ("Decode", &dec_ms),
        ("Bild zur CPU holen", &dl_ms),
    ] {
        summary(&format!(
            "| {name}, Median / p95 | {:.2} / {:.2} ms |",
            percentile(v.clone(), 0.5),
            percentile(v.clone(), 0.95)
        ));
    }
    summary(&format!(
        "| Keyframe / Ø Delta-Frame | {} / {avg_delta:.0} Bytes (Ziel {target:.0}) |",
        sizes[0]
    ));
    summary(&format!(
        "| PSNR (Y), schlechtester Frame | {min_psnr:.1} dB |\n"
    ));

    assert!(min_psnr > 30.0, "quality too low: {min_psnr:.1} dB");
    assert!(
        (target * 0.3..target * 1.5).contains(&avg_delta),
        "average delta frame {avg_delta:.0} B, target {target:.0} B"
    );
    assert!(percentile(enc_ms, 0.95) < 16.0);
    assert!(percentile(dec_ms, 0.95) < 16.0);
}

#[test]
fn keyframe_on_request_and_decoder_recovers() {
    if !nvidia() {
        return;
    }
    let (w, h) = (1280, 720);
    let mut enc = NvencEncoder::new(&config(w, h, 60, 8_000)).unwrap();
    let input = frames(w, h, 30);
    let mut packets = Vec::new();
    for (i, f) in input.iter().enumerate() {
        if i == 20 {
            enc.request_keyframe();
        }
        let mut out = EncodedFrame::default();
        enc.encode(f, &mut out).unwrap();
        packets.push(out);
    }
    let keyframes: Vec<usize> = (0..packets.len())
        .filter(|&i| packets[i].keyframe)
        .collect();
    assert_eq!(keyframes, vec![0, 20]);

    // A decoder joining at the requested keyframe (as after loss) decodes
    // from there on.
    let mut dec = NvdecDecoder::new(0).unwrap();
    let mut d = DecodedFrame::default();
    for p in &packets[20..] {
        dec.decode(&p.data, &mut d).unwrap();
    }
    let last = dec.last_frame_nv12().unwrap();
    let psnr = psnr_y(&input[29].data, &last, (w * h) as usize);
    assert!(psnr > 30.0, "{psnr:.1} dB after joining at the keyframe");
}

#[test]
fn rejects_wrong_input_and_garbage() {
    if !nvidia() {
        return;
    }
    let mut enc = NvencEncoder::new(&config(640, 360, 30, 1_000)).unwrap();
    let mut out = EncodedFrame::default();
    let wrong_size = frames(320, 180, 1).remove(0);
    assert!(enc.encode(&wrong_size, &mut out).is_err());
    assert!(NvencEncoder::new(&config(641, 360, 30, 1_000)).is_err());

    let mut dec = NvdecDecoder::new(0).unwrap();
    let mut d = DecodedFrame::default();
    assert!(dec.decode(&[], &mut d).is_err());
    assert!(dec.decode(&[0, 0, 0, 1, 0x65, 0xff, 0x00], &mut d).is_err());
    assert!(
        dec.picture(PictureKind::Nv12).unwrap().is_none(),
        "nothing decoded yet"
    );
    // Still works after garbage.
    for f in frames(640, 360, 3) {
        enc.encode(&f, &mut out).unwrap();
        dec.decode(&out.data, &mut d).unwrap();
    }
    assert_eq!((d.width, d.height), (640, 360));
}

#[test]
fn many_encoders_can_be_created_and_dropped() {
    if !nvidia() {
        return;
    }
    // Sessions come and go; leaked NVENC sessions would hit the driver's
    // limit quickly.
    let f = frames(640, 360, 1).remove(0);
    for _ in 0..30 {
        let mut enc = NvencEncoder::new(&config(640, 360, 60, 2_000)).unwrap();
        let mut dec = NvdecDecoder::new(0).unwrap();
        let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
        enc.encode(&f, &mut out).unwrap();
        dec.decode(&out.data, &mut d).unwrap();
    }
}

#[test]
fn without_an_nvidia_driver_creation_fails_cleanly() {
    if std::path::Path::new("/dev/nvidiactl").exists() {
        eprintln!("skipped: this machine has the NVIDIA driver");
        return;
    }
    let enc = NvencEncoder::new(&config(640, 360, 30, 1_000));
    assert!(matches!(enc, Err(CodecError::Backend(_))));
    let dec = NvdecDecoder::new(0);
    assert!(matches!(dec, Err(CodecError::Backend(_))));
}
