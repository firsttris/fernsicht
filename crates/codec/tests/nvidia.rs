//! Hardware tests for NVENC/NVDEC. They need an NVIDIA GPU and run only
//! when `FERNSICHT_GPU=nvidia` (the GPU CI job on the NVIDIA runner);
//! otherwise only the test without a driver runs. Measurements are
//! appended to the CI job summary.
#![cfg(feature = "nvidia")]

use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use fernsicht_capture::dmabuf::{DmaBuf, formats, fourcc, fourcc_name};
use fernsicht_gpu::Gpu;
use fernsicht_gpu::testimage::upload_as_dmabuf;

use fernsicht_capture::{Frame, FrameSource, TestPattern};
use fernsicht_codec::nvidia::{NvdecDecoder, NvencConfig, NvencEncoder};
use fernsicht_codec::{
    CodecError, DecodedFrame, Decoder, EncodedFrame, Encoder, Picture, PictureKind,
};
use fernsicht_proto::Codec;

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
        codec: Codec::H264,
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

// --- Screen input: RGB DMA-BUF (what KMS capture delivers) ----------------

const PATCHES: [(u8, u8, u8); 6] = [
    (255, 0, 0),
    (0, 255, 0),
    (0, 0, 255),
    (255, 255, 255),
    (16, 16, 16),
    (128, 128, 128),
];

fn pack(format: u32, r: u8, g: u8, b: u8) -> [u8; 4] {
    let ten = |v: u8| (u32::from(v) * 1023 + 127) / 255;
    match format {
        formats::XRGB8888 | formats::ARGB8888 => [b, g, r, 255],
        formats::XBGR8888 | formats::ABGR8888 => [r, g, b, 255],
        formats::XRGB2101010 | formats::ARGB2101010 => {
            (3 << 30 | ten(r) << 20 | ten(g) << 10 | ten(b)).to_le_bytes()
        }
        formats::XBGR2101010 | formats::ABGR2101010 => {
            (3 << 30 | ten(b) << 20 | ten(g) << 10 | ten(r)).to_le_bytes()
        }
        other => panic!("no packing for {}", fourcc_name(other)),
    }
}

/// Colour patches on top, a grey ramp below that moves with `shift`.
fn rgb_image(w: usize, h: usize, shift: usize, format: u32) -> Vec<u8> {
    let mut img = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = if y < h / 2 {
                PATCHES[x * PATCHES.len() / w]
            } else {
                let v = (((x + shift) % w) * 255 / w) as u8;
                (v, v, v)
            };
            img[(y * w + x) * 4..][..4].copy_from_slice(&pack(format, r, g, b));
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

/// Mean Y, Cb, Cr in the middle of every patch, against BT.709 (with room
/// for H.264 compression).
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

fn vulkan() -> Arc<Gpu> {
    Arc::new(Gpu::new().expect("Vulkan on the NVIDIA GPU"))
}

fn upload(gpu: &Arc<Gpu>, w: u32, h: u32, shift: usize, format: u32) -> DmaBuf {
    let img = rgb_image(w as usize, h as usize, shift, format);
    upload_as_dmabuf(gpu, w, h, format, &img).unwrap()
}

fn dmabuf_frame(image: &DmaBuf, seq: u64) -> Frame {
    let mut f = Frame::from_dmabuf(image.clone());
    f.seq = seq;
    f
}

#[test]
fn dmabuf_without_a_cpu_copy_bt709_and_latency() {
    if !nvidia() {
        return;
    }
    let gpu = vulkan();
    let (w, h) = (1920u32, 1080u32);
    let images = [0, 64].map(|shift| upload(&gpu, w, h, shift, formats::XRGB8888));
    let mut enc = NvencEncoder::new(&config(w, h, 60, 20_000)).unwrap();
    let mut dec = NvdecDecoder::new(0).unwrap();
    let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
    let mut enc_ms = Vec::new();
    for i in 0..120u64 {
        let frame = dmabuf_frame(&images[(i % 2) as usize], i);
        let t = Instant::now();
        enc.encode(&frame, &mut out).unwrap();
        enc_ms.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(out.keyframe, i == 0);
        dec.decode(&out.data, &mut d).unwrap();
    }
    assert_eq!((d.width, d.height), (w, h));
    let nv12 = dec.last_frame_nv12().unwrap();
    assert_patch_colours(&nv12, w as usize, h as usize, "XR24 1080p");

    // The first frame paid for setting up Vulkan and CUDA.
    let steady = enc_ms[1..].to_vec();
    summary("### NVENC H.264 aus DMA-BUF (Vulkan → CUDA, ohne CPU-Kopie), 1080p60");
    summary("| Messung | Wert |\n|---|---|");
    summary(&format!(
        "| Import + RGB→NV12 + Kopie + Encode, Median / p95 | {:.2} / {:.2} ms |",
        percentile(steady.clone(), 0.5),
        percentile(steady.clone(), 0.95)
    ));
    summary(&format!(
        "| Erster Frame (Einrichtung) | {:.1} ms |",
        enc_ms[0]
    ));
    summary(&format!(
        "| Modifier des Testbilds | {:#x} |\n",
        images[0].modifier
    ));
    assert!(percentile(steady, 0.95) < 16.0);
}

#[test]
fn every_desktop_format_keeps_its_colours_through_nvenc() {
    if !nvidia() {
        return;
    }
    let gpu = vulkan();
    let (w, h) = (1280u32, 720u32);
    for format in [
        formats::XRGB8888,
        formats::ARGB8888,
        formats::XBGR8888,
        formats::ABGR8888,
        formats::XRGB2101010,
        formats::ARGB2101010,
        formats::XBGR2101010,
        formats::ABGR2101010,
    ] {
        let name = fourcc_name(format);
        let image = upload(&gpu, w, h, 0, format);
        let mut enc = NvencEncoder::new(&config(w, h, 60, 8_000)).unwrap();
        let mut dec = NvdecDecoder::new(0).unwrap();
        let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
        for i in 0..3 {
            enc.encode(&dmabuf_frame(&image, i), &mut out)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            dec.decode(&out.data, &mut d).unwrap();
        }
        assert_patch_colours(
            &dec.last_frame_nv12().unwrap(),
            w as usize,
            h as usize,
            &name,
        );
        summary(&format!("- NVENC aus DMA-BUF {name}: Farben ok"));
    }
}

#[test]
fn screen_is_scaled_and_cpu_and_dmabuf_input_mix() {
    if !nvidia() {
        return;
    }
    let gpu = vulkan();
    let (w, h) = (1280u32, 720u32);
    let mut enc = NvencEncoder::new(&config(w, h, 60, 8_000)).unwrap();
    let mut dec = NvdecDecoder::new(0).unwrap();
    let big = upload(&gpu, 2560, 1440, 0, formats::ABGR2101010);
    let small = upload(&gpu, w, h, 0, formats::XRGB8888);
    let cpu = frames(w, h, 1).remove(0);
    let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
    for (i, f) in [
        cpu.clone(),
        dmabuf_frame(&big, 1),
        dmabuf_frame(&small, 2),
        cpu,
        dmabuf_frame(&big, 4),
    ]
    .iter()
    .enumerate()
    {
        enc.encode(f, &mut out)
            .unwrap_or_else(|e| panic!("input {i}: {e}"));
        dec.decode(&out.data, &mut d).unwrap();
        assert_eq!((d.width, d.height), (w, h));
    }
    assert_patch_colours(
        &dec.last_frame_nv12().unwrap(),
        w as usize,
        h as usize,
        "1440p AB30 scaled to 720p",
    );
}

#[test]
fn bad_dmabufs_are_rejected_and_the_encoder_keeps_working() {
    if !nvidia() {
        return;
    }
    let gpu = vulkan();
    let mut enc = NvencEncoder::new(&config(640, 360, 30, 1_000)).unwrap();
    let mut out = EncodedFrame::default();
    let good = upload(&gpu, 640, 360, 0, formats::XRGB8888);
    let not_a_dmabuf = DmaBuf {
        objects: vec![Arc::new(std::fs::File::open("/dev/null").unwrap().into())],
        ..good.clone()
    };
    let yuyv = DmaBuf {
        fourcc: fourcc(b"YUYV"),
        ..good.clone()
    };
    for (name, bad) in [("/dev/null", not_a_dmabuf), ("YUYV", yuyv)] {
        assert!(
            enc.encode(&dmabuf_frame(&bad, 0), &mut out).is_err(),
            "{name} must be rejected"
        );
    }
    enc.encode(&dmabuf_frame(&good, 1), &mut out).unwrap();
    assert!(out.keyframe, "the first good frame is the keyframe");
}

// --- HEVC ------------------------------------------------------------------

/// NAL unit types in an Annex-B HEVC access unit.
fn hevc_nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            types.push((data[i + 3] >> 1) & 0x3f);
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}

const HEVC_VPS: u8 = 32;
const HEVC_SPS: u8 = 33;
const HEVC_PPS: u8 = 34;

/// Encodes `input` and returns (mean bytes per delta frame, worst PSNR).
fn quality(codec: Codec, input: &[Frame], kbps: u32) -> (f64, f64) {
    let f0 = &input[0];
    let mut enc = NvencEncoder::new(&NvencConfig {
        codec,
        ..config(f0.width, f0.height, 60, kbps)
    })
    .unwrap();
    let mut dec = NvdecDecoder::for_codec(0, codec).unwrap();
    let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
    let (mut bytes, mut worst) = (0usize, f64::INFINITY);
    for (i, f) in input.iter().enumerate() {
        enc.encode(f, &mut out).unwrap();
        if i > 0 {
            bytes += out.data.len();
        }
        dec.decode(&out.data, &mut d).unwrap();
        let luma = (f.width * f.height) as usize;
        worst = worst.min(psnr_y(&f.data, &dec.last_frame_nv12().unwrap(), luma));
    }
    (bytes as f64 / (input.len() - 1) as f64, worst)
}

#[test]
fn hevc_keyframes_carry_parameter_sets_and_a_late_decoder_joins() {
    if !nvidia() {
        return;
    }
    let (w, h) = (1280, 720);
    let mut enc = NvencEncoder::new(&NvencConfig {
        codec: Codec::Hevc,
        ..config(w, h, 60, 8_000)
    })
    .unwrap();
    assert_eq!(enc.codec(), Codec::Hevc);
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
    for k in keyframes {
        let types = hevc_nal_types(&packets[k].data);
        for t in [HEVC_VPS, HEVC_SPS, HEVC_PPS] {
            assert!(types.contains(&t), "keyframe {k} lacks NAL {t}: {types:?}");
        }
    }
    let mut dec = NvdecDecoder::for_codec(0, Codec::Hevc).unwrap();
    let mut d = DecodedFrame::default();
    for p in &packets[20..] {
        dec.decode(&p.data, &mut d).unwrap();
    }
    let psnr = psnr_y(
        &input[29].data,
        &dec.last_frame_nv12().unwrap(),
        (w * h) as usize,
    );
    assert!(psnr > 30.0, "{psnr:.1} dB after joining at the keyframe");
    let mut h264 = NvdecDecoder::new(0).unwrap();
    assert!(h264.decode(&packets[0].data, &mut d).is_err());
}

#[test]
fn hevc_is_sharper_than_h264_at_the_same_bitrate() {
    if !nvidia() {
        return;
    }
    let input = frames(1920, 1080, 60);
    let (h264_bytes, h264_psnr) = quality(Codec::H264, &input, 4_000);
    let (hevc_bytes, hevc_psnr) = quality(Codec::Hevc, &input, 4_000);
    summary("### H.264 und HEVC bei gleicher Bitrate (NVENC, 1080p60, 4 Mbit/s)");
    summary("| Codec | Ø Delta-Frame | PSNR (Y), schlechtester Frame |\n|---|---|---|");
    summary(&format!(
        "| H.264 | {h264_bytes:.0} Bytes | {h264_psnr:.1} dB |"
    ));
    summary(&format!(
        "| HEVC | {hevc_bytes:.0} Bytes | {hevc_psnr:.1} dB |\n"
    ));
    assert!(
        hevc_psnr > h264_psnr - 0.5,
        "HEVC {hevc_psnr:.1} dB vs H.264 {h264_psnr:.1} dB"
    );
}

#[test]
fn hevc_from_a_dmabuf_keeps_its_colours() {
    if !nvidia() {
        return;
    }
    let gpu = vulkan();
    let (w, h) = (1920u32, 1080u32);
    let image = upload(&gpu, w, h, 0, formats::ABGR2101010);
    let mut enc = NvencEncoder::new(&NvencConfig {
        codec: Codec::Hevc,
        ..config(w, h, 60, 20_000)
    })
    .unwrap();
    let mut dec = NvdecDecoder::for_codec(0, Codec::Hevc).unwrap();
    let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
    let mut ms = Vec::new();
    for i in 0..60 {
        let t = Instant::now();
        enc.encode(&dmabuf_frame(&image, i), &mut out).unwrap();
        ms.push(t.elapsed().as_secs_f64() * 1e3);
        dec.decode(&out.data, &mut d).unwrap();
    }
    assert_patch_colours(
        &dec.last_frame_nv12().unwrap(),
        w as usize,
        h as usize,
        "HEVC AB30",
    );
    let p95 = percentile(ms[1..].to_vec(), 0.95);
    summary(&format!(
        "- HEVC aus DMA-BUF (AB30, 1080p): Farben ok, Encode Median / p95 {:.2} / {p95:.2} ms\n",
        percentile(ms[1..].to_vec(), 0.5)
    ));
    assert!(p95 < 16.0);
}

#[test]
fn nvdec_reports_what_it_decodes() {
    if !nvidia() {
        return;
    }
    // Pascal and newer decode both in hardware.
    assert!(fernsicht_codec::nvidia::decodes(0, Codec::H264));
    assert!(fernsicht_codec::nvidia::decodes(0, Codec::Hevc));
    assert!(!fernsicht_codec::nvidia::decodes(0, Codec::Synthetic));
    assert!(
        !fernsicht_codec::nvidia::decodes(99, Codec::H264),
        "no such GPU"
    );
}

#[test]
fn av1_is_offered_only_where_nvidia_has_it() {
    if !nvidia() {
        return;
    }
    // Decoding from RTX 30, encoding from RTX 40. What the GPU reports and
    // what opening a decoder or encoder does must agree: the client offers
    // AV1 by the report, the host falls back when the encoder fails.
    let decodes = fernsicht_codec::nvidia::decodes(0, Codec::Av1);
    summary(&format!(
        "- NVDEC dekodiert AV1: {}",
        if decodes { "ja" } else { "nein" }
    ));
    match NvencEncoder::new(&NvencConfig {
        codec: Codec::Av1,
        ..config(1280, 720, 60, 8_000)
    }) {
        Ok(mut enc) => {
            let f = frames(1280, 720, 1).remove(0);
            let mut out = EncodedFrame::default();
            enc.encode(&f, &mut out).unwrap();
            assert!(out.keyframe);
            summary("- NVENC kodiert AV1: ja");
        }
        Err(e) => {
            // Refused for want of hardware, not for our settings.
            assert!(!decodes || e.to_string().contains("av1_nvenc"), "{e}");
            summary("- NVENC kodiert AV1: nein (der Host nimmt dann HEVC oder H.264)");
        }
    }
}
