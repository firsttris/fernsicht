//! Hardware tests for Vulkan Video (FFmpeg's Vulkan encoder and decoder).
//! They need a GPU whose driver has Vulkan Video and run only when
//! `FERNSICHT_GPU` names one (the GPU CI jobs); the render node is
//! `FERNSICHT_RENDER_NODE` or `/dev/dri/renderD128`. Measurements are
//! appended to the CI job summary.
#![cfg(feature = "vulkan")]

mod common;

use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use fernsicht_capture::dmabuf::{DmaBuf, formats};
use fernsicht_capture::{Frame, FrameSource, TestPattern};
use fernsicht_codec::vulkan::{
    VulkanDecoder, VulkanEncoder, VulkanEncoderConfig, decodes, decodes_in, encodes_in,
    ffmpeg_is_ready, gpu_encodes,
};
use fernsicht_codec::{DecodedFrame, Decoder, EncodedFrame, Encoder, Picture, PictureKind};
use fernsicht_core::colour::{Colour, Depth};
use fernsicht_gpu::testimage::upload_as_dmabuf;
use fernsicht_gpu::{Gpu, Prefer};
use fernsicht_proto::Codec;

fn render_node() -> String {
    std::env::var("FERNSICHT_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into())
}

fn gpu() -> Arc<Gpu> {
    Arc::new(Gpu::preferring(Prefer::render_node(&render_node())).expect("Vulkan on the GPU"))
}

/// Whether to run: a GPU is named and it encodes `codec` with Vulkan
/// Video.
fn ready(codec: Codec) -> bool {
    if std::env::var("FERNSICHT_GPU").is_err() {
        eprintln!("skipped: set FERNSICHT_GPU (needs a GPU with Vulkan Video)");
        return false;
    }
    if !ffmpeg_is_ready() {
        eprintln!("skipped: FFmpeg before 7.1");
        return false;
    }
    let gpu = gpu();
    // Encoded here, and decoded again to check it.
    if !(gpu_encodes(&gpu, codec) && decodes(&gpu, codec)) {
        eprintln!(
            "skipped: {} does not encode and decode {codec:?} with Vulkan Video",
            gpu.name()
        );
        return false;
    }
    true
}

fn config(codec: Codec, width: u32, height: u32, kbps: u32) -> VulkanEncoderConfig {
    VulkanEncoderConfig {
        render_node: render_node(),
        codec,
        width,
        height,
        fps: 60,
        bitrate_kbps: kbps,
        colour: Colour::SDR,
    }
}

fn decoder(codec: Codec) -> VulkanDecoder {
    VulkanDecoder::for_codec(gpu(), codec).unwrap()
}

fn download(dec: &mut VulkanDecoder) -> Vec<u8> {
    match dec.picture(PictureKind::Nv12).unwrap().unwrap() {
        Picture::Nv12 { data, .. } => data.to_vec(),
        other => panic!("asked for a CPU picture, got {other:?}"),
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

/// Encodes `input`, decodes every frame; returns encode and decode times,
/// frame sizes, keyframe positions and the worst PSNR of every tenth frame.
fn roundtrip(codec: Codec, input: &[Frame], kbps: u32) -> Roundtrip {
    let (w, h) = (input[0].width, input[0].height);
    let mut enc = VulkanEncoder::new(&config(codec, w, h, kbps)).unwrap();
    let mut dec = decoder(codec);
    let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
    let mut r = Roundtrip::default();
    for (i, f) in input.iter().enumerate() {
        let t = Instant::now();
        // encode() fails if the encoder held the frame back.
        enc.encode(f, &mut out).unwrap();
        r.enc_ms.push(t.elapsed().as_secs_f64() * 1e3);
        if out.keyframe {
            r.keyframes.push(i);
        }
        r.sizes.push(out.data.len());
        let t = Instant::now();
        dec.decode(&out.data, &mut d).unwrap();
        r.dec_ms.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!((d.width, d.height), (w, h));
        if i % 10 == 9 {
            let psnr = psnr_y(&f.data, &download(&mut dec), (w * h) as usize);
            r.min_psnr = r.min_psnr.min(psnr);
        }
    }
    r.bt709 = dec.last_frame_is_bt709_limited();
    r
}

struct Roundtrip {
    enc_ms: Vec<f64>,
    dec_ms: Vec<f64>,
    sizes: Vec<usize>,
    keyframes: Vec<usize>,
    min_psnr: f64,
    bt709: bool,
}

impl Default for Roundtrip {
    fn default() -> Self {
        Self {
            enc_ms: Vec::new(),
            dec_ms: Vec::new(),
            sizes: Vec::new(),
            keyframes: Vec::new(),
            min_psnr: f64::INFINITY,
            bt709: false,
        }
    }
}

#[test]
fn roundtrip_1080p60_quality_latency_and_framing() {
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        if !ready(codec) {
            continue;
        }
        let (w, h, kbps) = (1920, 1080, 20_000);
        let r = roundtrip(codec, &frames(w, h, 120), kbps);
        let avg_delta = r.sizes[1..].iter().sum::<usize>() as f64 / (r.sizes.len() - 1) as f64;
        let target = f64::from(kbps) * 1000.0 / 8.0 / 60.0;
        summary(&format!(
            "### Vulkan Video {codec:?} 1080p60, 20 Mbit/s (120 Frames, {})",
            gpu().name()
        ));
        summary("| Messung | Wert |\n|---|---|");
        for (name, v) in [("Encode inkl. Upload", &r.enc_ms), ("Decode", &r.dec_ms)] {
            summary(&format!(
                "| {name}, Median / p95 | {:.2} / {:.2} ms |",
                percentile(v.clone(), 0.5),
                percentile(v.clone(), 0.95)
            ));
        }
        summary(&format!(
            "| Keyframe / Ø Delta-Frame | {} / {avg_delta:.0} Bytes (Ziel {target:.0}) |",
            r.sizes[0]
        ));
        summary(&format!(
            "| PSNR (Y), schlechtester Frame | {:.1} dB |\n",
            r.min_psnr
        ));
        assert_eq!(r.keyframes, [0], "{codec:?}: only the first is a keyframe");
        assert!(r.bt709, "{codec:?}: colour not signalled");
        assert!(r.min_psnr > 30.0, "{codec:?}: {:.1} dB", r.min_psnr);
        assert!(
            (target * 0.3..target * 1.5).contains(&avg_delta),
            "{codec:?}: average delta frame {avg_delta:.0} B, target {target:.0} B"
        );
        assert!(percentile(r.enc_ms, 0.95) < 16.0);
        assert!(percentile(r.dec_ms, 0.95) < 16.0);
    }
}

#[test]
fn keyframe_on_request_and_the_bitrate_changes() {
    if !ready(Codec::H264) {
        return;
    }
    let (w, h) = (1280, 720);
    let mut enc = VulkanEncoder::new(&config(Codec::H264, w, h, 8_000)).unwrap();
    let input = frames(w, h, 120);
    let mut packets = Vec::new();
    for (i, f) in input.iter().enumerate() {
        if i == 20 {
            enc.request_keyframe();
        }
        if i == 60 {
            enc.set_bitrate(2_000).unwrap();
        }
        let mut out = EncodedFrame::default();
        enc.encode(f, &mut out).unwrap();
        packets.push(out);
    }
    let keyframes: Vec<usize> = (0..packets.len())
        .filter(|&i| packets[i].keyframe)
        .collect();
    // The new rate control starts with a keyframe.
    assert_eq!(keyframes, [0, 20, 60]);
    let avg = |r: std::ops::Range<usize>| {
        packets[r.clone()]
            .iter()
            .map(|p| p.data.len())
            .sum::<usize>() as f64
            / r.len() as f64
    };
    let (before, after) = (avg(30..60), avg(70..120));
    assert!(
        after < before * 0.5,
        "{before:.0} → {after:.0} bytes per frame"
    );

    // A decoder joining at the requested keyframe (as after loss) decodes
    // from there on, also across the bitrate change.
    let mut dec = decoder(Codec::H264);
    let mut d = DecodedFrame::default();
    for p in &packets[20..] {
        dec.decode(&p.data, &mut d).unwrap();
    }
    let psnr = psnr_y(&input[119].data, &download(&mut dec), (w * h) as usize);
    assert!(psnr > 28.0, "{psnr:.1} dB at 2 Mbit/s");
}

#[test]
fn wrong_input_is_an_error_and_the_encoder_goes_on() {
    if !ready(Codec::H264) {
        return;
    }
    let mut enc = VulkanEncoder::new(&config(Codec::H264, 640, 360, 2_000)).unwrap();
    let mut out = EncodedFrame::default();
    let wrong = frames(320, 180, 1);
    assert!(enc.encode(&wrong[0], &mut out).is_err());
    let right = frames(640, 360, 1);
    enc.encode(&right[0], &mut out).unwrap();
    assert!(out.keyframe && !out.data.is_empty());
    assert!(VulkanEncoder::new(&config(Codec::H264, 641, 360, 2_000)).is_err());
}

// --- Screen input: RGB DMA-BUF (what KMS capture delivers) ----------------

const PATCHES: [(u8, u8, u8); 6] = [
    (255, 0, 0),
    (0, 255, 0),
    (0, 0, 255),
    (255, 255, 255),
    (16, 16, 16),
    (128, 128, 128),
];

/// Colour patches on top, a grey ramp below that moves with `shift`
/// (XRGB8888: B, G, R, X in memory).
fn rgb_image(w: usize, h: usize, shift: usize) -> Vec<u8> {
    let mut img = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = if y < h / 2 {
                PATCHES[x * PATCHES.len() / w]
            } else {
                let v = (((x + shift) % w) * 255 / w) as u8;
                (v, v, v)
            };
            img[(y * w + x) * 4..][..4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    img
}

fn bt709(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (r, g, b) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let (kr, kb) = (0.2126, 0.0722);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    (
        16.0 + 219.0 * y,
        128.0 + 224.0 * (b - y) / (2.0 * (1.0 - kb)),
        128.0 + 224.0 * (r - y) / (2.0 * (1.0 - kr)),
    )
}

/// Mean Y, Cb, Cr in the middle of every patch, against BT.709.
fn assert_patch_colours(nv12: &[u8], w: usize, h: usize, what: &str) {
    let pw = w / PATCHES.len();
    for (i, &(r, g, b)) in PATCHES.iter().enumerate() {
        let (x0, x1) = (i * pw + pw / 4, (i + 1) * pw - pw / 4);
        let (y0, y1) = (h / 8, h * 3 / 8);
        let (mut ys, mut cbs, mut crs, mut n, mut nc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for y in y0..y1 {
            for x in x0..x1 {
                ys += f64::from(nv12[y * w + x]);
                n += 1.0;
                if y % 2 == 0 && x % 2 == 0 {
                    let uv = w * h + (y / 2) * w + x;
                    cbs += f64::from(nv12[uv]);
                    crs += f64::from(nv12[uv + 1]);
                    nc += 1.0;
                }
            }
        }
        let got = (ys / n, cbs / nc, crs / nc);
        let want = bt709(r, g, b);
        assert!(
            (got.0 - want.0).abs() < 4.0
                && (got.1 - want.1).abs() < 5.0
                && (got.2 - want.2).abs() < 5.0,
            "{what}, patch {i} RGB({r},{g},{b}): got YCbCr {got:.1?}, BT.709 wants {want:.1?}"
        );
    }
}

fn upload(gpu: &Arc<Gpu>, w: u32, h: u32, shift: usize) -> DmaBuf {
    let img = rgb_image(w as usize, h as usize, shift);
    upload_as_dmabuf(gpu, w, h, formats::XRGB8888, &img).unwrap()
}

#[test]
fn the_screen_goes_to_the_encoder_without_leaving_the_gpu() {
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
        if !ready(codec) {
            continue;
        }
        let gpu = gpu();
        let (w, h) = (1920u32, 1080u32);
        let images = [0, 64].map(|shift| upload(&gpu, w, h, shift));
        let mut enc = VulkanEncoder::new(&config(codec, w, h, 20_000)).unwrap();
        let mut dec = decoder(codec);
        let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
        let mut enc_ms = Vec::new();
        for i in 0..120u64 {
            let mut frame = Frame::from_dmabuf(images[(i % 2) as usize].clone());
            frame.seq = i;
            let t = Instant::now();
            enc.encode(&frame, &mut out).unwrap();
            enc_ms.push(t.elapsed().as_secs_f64() * 1e3);
            assert_eq!(out.keyframe, i == 0);
            dec.decode(&out.data, &mut d).unwrap();
        }
        assert_eq!((d.width, d.height), (w, h));
        assert_eq!(enc.cached_imports(), 2, "each screen buffer imported once");
        assert_patch_colours(&download(&mut dec), w as usize, h as usize, "XR24 1080p");

        // The first frame paid for setting up the converter.
        let steady = enc_ms[1..].to_vec();
        summary(&format!(
            "### Vulkan Video {codec:?} aus DMA-BUF (alles auf der GPU), 1080p60 ({})",
            gpu.name()
        ));
        summary(&format!(
            "| Import + RGB→NV12 + Kopie + Encode, Median / p95 | {:.2} / {:.2} ms |\n",
            percentile(steady.clone(), 0.5),
            percentile(steady.clone(), 0.95)
        ));
        assert!(percentile(steady, 0.95) < 16.0);
    }
}

#[test]
fn a_ten_bit_screen_goes_through_vulkan_video_in_hevc_main_10() {
    if !ready(Codec::Hevc) {
        return;
    }
    let gpu = gpu();
    if !(encodes_in(&render_node(), Codec::Hevc, Depth::Ten)
        && decodes_in(&gpu, Codec::Hevc, Depth::Ten))
    {
        eprintln!("skipped: {} has no Vulkan Video HEVC Main 10", gpu.name());
        return;
    }
    let (w, h) = (1280u32, 720u32);
    let img = common::xr30_patches(w as usize, h as usize, &PATCHES);
    let image = upload_as_dmabuf(&gpu, w, h, formats::XRGB2101010, &img).unwrap();
    for colour in [Colour::SDR.with_depth(Depth::Ten), Colour::HDR10] {
        let mut enc = VulkanEncoder::new(&VulkanEncoderConfig {
            colour,
            ..config(Codec::Hevc, w, h, 8_000)
        })
        .unwrap();
        let mut dec = decoder(Codec::Hevc);
        let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
        for i in 0..3 {
            let mut frame = Frame::from_dmabuf(image.clone());
            frame.seq = i;
            enc.encode(&frame, &mut out).unwrap();
            dec.decode(&out.data, &mut d).unwrap();
        }
        match dec.picture(PictureKind::Gpu).unwrap() {
            Some(Picture::Vulkan(p)) => assert_eq!(p.colour, colour),
            other => panic!("no Vulkan picture: {other:?}"),
        }
        let p010 = download(&mut dec);
        assert_eq!(p010.len(), (w * h * 3) as usize, "P010");
        common::assert_patches(
            &p010,
            (w as usize, h as usize),
            &PATCHES,
            colour,
            4.0,
            &format!("XR30 → Vulkan Video HEVC Main 10, {colour:?}"),
        );
        summary(&format!(
            "- Vulkan Video HEVC Main 10 aus XR30, {colour:?} ({}): Farben ok",
            gpu.name()
        ));
    }
}
