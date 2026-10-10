//! Hardware tests for the VAAPI backend. They need a GPU and run only when
//! `FERNSICHT_GPU` is `amd` or `intel` (set by the GPU CI job on the
//! self-hosted runner, see docs/gpu-runner.md); otherwise they skip.
//!
//! `FERNSICHT_RENDER_NODE` overrides the render node (default
//! /dev/dri/renderD128). Measurements are appended to the CI job summary.
#![cfg(feature = "vaapi")]

use std::io::Write;
use std::time::Instant;

use fernsicht_capture::{Frame, FrameSource, PixelFormat, TestPattern};
use fernsicht_codec::vaapi::{DEFAULT_RENDER_NODE, VaapiDecoder, VaapiEncoder, VaapiEncoderConfig};
use fernsicht_codec::{CodecError, DecodedFrame, Decoder, EncodedFrame, Encoder};

fn render_node() -> Option<String> {
    match std::env::var("FERNSICHT_GPU").as_deref() {
        Ok("amd" | "intel") => Some(
            std::env::var("FERNSICHT_RENDER_NODE").unwrap_or_else(|_| DEFAULT_RENDER_NODE.into()),
        ),
        _ => {
            eprintln!("skipped: set FERNSICHT_GPU=amd (needs a VAAPI GPU)");
            None
        }
    }
}

fn config(node: &str, width: u32, height: u32, fps: u32, kbps: u32) -> VaapiEncoderConfig {
    VaapiEncoderConfig {
        render_node: node.into(),
        width,
        height,
        fps,
        bitrate_kbps: kbps,
    }
}

/// Test frames without the pacing sleep of a real 60 fps source.
fn frames(width: u32, height: u32, count: usize) -> Vec<Frame> {
    let mut src = TestPattern::new(width, height, 100_000);
    (0..count)
        .map(|_| {
            let mut f = src.alloc_frame();
            src.next_frame(&mut f).unwrap();
            // Add texture so the encoder has real work: a diagonal gradient
            // that moves with the bar.
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
    let Some(node) = render_node() else { return };
    let (w, h, fps, kbps) = (1920, 1080, 60, 20_000);
    let mut enc = VaapiEncoder::new(&config(&node, w, h, fps, kbps)).unwrap();
    let mut dec = VaapiDecoder::new(&node).unwrap();
    let input = frames(w, h, 120);
    let mut out = EncodedFrame::default();
    let mut decoded = DecodedFrame::default();
    let (mut enc_ms, mut dec_ms, mut sizes, mut psnrs) = (vec![], vec![], vec![], vec![]);

    for (i, f) in input.iter().enumerate() {
        let t = Instant::now();
        // One packet per frame is enforced by encode() itself: it fails if
        // the encoder held the frame back (that would be a frame of latency).
        enc.encode(f, &mut out).unwrap();
        enc_ms.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(
            out.keyframe,
            i == 0,
            "frame {i}: only the first frame is a keyframe"
        );
        sizes.push(out.data.len());

        let t = Instant::now();
        dec.decode(&out.data, &mut decoded).unwrap();
        dec_ms.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!((decoded.width, decoded.height), (w, h));
        assert_eq!(
            decoded.seq, i as u64,
            "decoder emits one frame per packet, in order"
        );
        if i % 10 == 0 {
            let pixels = dec.last_frame_nv12().unwrap();
            psnrs.push(psnr_y(&f.data, &pixels, (w * h) as usize));
        }
    }

    let min_psnr = psnrs.iter().copied().fold(f64::INFINITY, f64::min);
    let delta: Vec<usize> = sizes[1..].to_vec();
    let avg_delta = delta.iter().sum::<usize>() as f64 / delta.len() as f64;
    let target = f64::from(kbps) * 1000.0 / 8.0 / f64::from(fps);
    summary("### VAAPI H.264 1080p60, 20 Mbit/s (120 Frames)");
    summary("| Messung | Wert |\n|---|---|");
    summary(&format!(
        "| Encode inkl. Upload, Median / p95 | {:.2} / {:.2} ms |",
        percentile(enc_ms.clone(), 0.5),
        percentile(enc_ms.clone(), 0.95)
    ));
    summary(&format!(
        "| Decode, Median / p95 | {:.2} / {:.2} ms |",
        percentile(dec_ms.clone(), 0.5),
        percentile(dec_ms.clone(), 0.95)
    ));
    summary(&format!(
        "| Keyframe / Ø Delta-Frame | {} / {:.0} Bytes (Ziel {:.0}) |",
        sizes[0], avg_delta, target
    ));
    summary(&format!(
        "| PSNR (Y), schlechtester Frame | {min_psnr:.1} dB |\n"
    ));

    assert!(min_psnr > 30.0, "quality too low: {min_psnr:.1} dB");
    // CBR with a one-frame buffer: delta frames close to the target size.
    assert!(
        (target * 0.3..target * 1.5).contains(&avg_delta),
        "average delta frame {avg_delta:.0} B, target {target:.0} B"
    );
    // Generous bounds: a 60 fps frame budget is 16.7 ms for the whole
    // pipeline; the exact numbers land in the summary above.
    assert!(percentile(enc_ms, 0.95) < 16.0);
    assert!(percentile(dec_ms, 0.95) < 16.0);
}

#[test]
fn keyframe_on_request_and_decoder_recovers() {
    let Some(node) = render_node() else { return };
    let (w, h) = (1280, 720);
    let mut enc = VaapiEncoder::new(&config(&node, w, h, 60, 8_000)).unwrap();
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
    let keyframes: Vec<usize> = packets
        .iter()
        .enumerate()
        .filter(|(_, p)| p.keyframe)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(keyframes, vec![0, 20]);

    // A decoder that joins at the requested keyframe (as after loss) decodes
    // from there on.
    let mut dec = VaapiDecoder::new(&node).unwrap();
    let mut d = DecodedFrame::default();
    for p in &packets[20..] {
        dec.decode(&p.data, &mut d).unwrap();
    }
    assert!(d.width == w && d.height == h);
    let last = dec.last_frame_nv12().unwrap();
    let psnr = psnr_y(&input[29].data, &last, (w * h) as usize);
    assert!(psnr > 30.0, "{psnr:.1} dB after joining at the keyframe");
}

#[test]
fn rejects_wrong_input_and_garbage() {
    let Some(node) = render_node() else { return };
    let mut enc = VaapiEncoder::new(&config(&node, 640, 360, 30, 1_000)).unwrap();
    let mut out = EncodedFrame::default();
    let wrong = Frame::new(320, 240, PixelFormat::Nv12);
    assert!(matches!(
        enc.encode(&wrong, &mut out),
        Err(CodecError::Backend(_))
    ));
    assert!(VaapiEncoder::new(&config(&node, 641, 360, 30, 1_000)).is_err());
    assert!(VaapiEncoder::new(&config("/dev/dri/does-not-exist", 640, 360, 30, 1_000)).is_err());

    let mut dec = VaapiDecoder::new(&node).unwrap();
    let mut d = DecodedFrame::default();
    assert!(dec.decode(&[], &mut d).is_err());
    for junk in [&[0u8, 0, 0, 1, 0x65, 0xff, 0x00][..], &[0x42; 4096][..]] {
        assert!(dec.decode(junk, &mut d).is_err(), "garbage must not decode");
    }
    // And the decoder still works afterwards.
    let input = frames(640, 360, 1);
    enc.request_keyframe();
    enc.encode(&input[0], &mut out).unwrap();
    dec.decode(&out.data, &mut d).unwrap();
}

#[test]
fn many_encoders_can_be_created_and_dropped() {
    // Leak check of the FFmpeg/VAAPI resources: sessions come and go.
    let Some(node) = render_node() else { return };
    let input = frames(320, 240, 2);
    for _ in 0..50 {
        let mut enc = VaapiEncoder::new(&config(&node, 320, 240, 30, 500)).unwrap();
        let mut dec = VaapiDecoder::new(&node).unwrap();
        let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
        for f in &input {
            enc.encode(f, &mut out).unwrap();
            dec.decode(&out.data, &mut d).unwrap();
        }
    }
}
