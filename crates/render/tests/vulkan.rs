//! The Vulkan renderer against reference colours: NV12 in, RGBA read back.
//!
//! Runs on any Vulkan 1.3 device; CI uses the software rasterizer
//! (llvmpipe). Without a device the tests skip, unless
//! `FERNSICHT_REQUIRE_VULKAN=1` (set in CI so they cannot skip silently).
//! With feature `vaapi` and `FERNSICHT_GPU=amd|intel`, pictures also come
//! from the VAAPI decoder, by DMA-BUF without a copy.
#![cfg(feature = "vulkan")]

use std::sync::{Arc, OnceLock};

use fernsicht_codec::Picture;
use fernsicht_render::vulkan::{Gpu, Renderer};

/// Colour stripes left to right, as (R, G, B).
const STRIPES: [(u8, u8, u8); 6] = [
    (255, 0, 0),
    (0, 255, 0),
    (0, 0, 255),
    (255, 255, 255),
    (16, 16, 16),
    (200, 120, 40),
];

/// One device for all tests of the process, like the client has one.
fn gpu() -> &'static Result<Arc<Gpu>, String> {
    static GPU: OnceLock<Result<Arc<Gpu>, String>> = OnceLock::new();
    GPU.get_or_init(|| {
        // RUST_LOG=fernsicht_render=debug shows the steps of the setup.
        let _ = env_logger::builder().is_test(true).try_init();
        let gpu = Gpu::new().map(Arc::new);
        if let Ok(g) = &gpu {
            eprintln!("Vulkan device: {}", g.name());
        }
        gpu
    })
}

/// A fresh renderer on the shared device.
fn renderer() -> Option<Renderer> {
    match gpu() {
        Ok(gpu) => Some(Renderer::new(gpu.clone()).expect("renderer")),
        Err(e) if std::env::var_os("FERNSICHT_REQUIRE_VULKAN").is_none() => {
            eprintln!("skipped, no Vulkan: {e}");
            None
        }
        Err(e) => panic!("FERNSICHT_REQUIRE_VULKAN is set but: {e}"),
    }
}

/// BT.709 limited range, what the encoder produces.
fn ycbcr(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let q = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    (
        q(16.0 + 219.0 * y),
        q(128.0 + 224.0 * (b - y) / 1.8556),
        q(128.0 + 224.0 * (r - y) / 1.5748),
    )
}

/// NV12 picture of the stripes.
fn stripes_nv12(w: usize, h: usize) -> Vec<u8> {
    let stripe = |x: usize| STRIPES[x * STRIPES.len() / w];
    let mut img = vec![0u8; w * h * 3 / 2];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = stripe(x);
            img[y * w + x] = ycbcr(r, g, b).0;
        }
    }
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let (r, g, b) = stripe(x * 2);
            let (_, cb, cr) = ycbcr(r, g, b);
            img[w * h + y * w + x * 2] = cb;
            img[w * h + y * w + x * 2 + 1] = cr;
        }
    }
    img
}

fn pixel(rgba: &[u8], w: usize, x: usize, y: usize) -> (u8, u8, u8) {
    let p = &rgba[(y * w + x) * 4..][..4];
    (p[0], p[1], p[2])
}

/// The middle of every stripe matches its colour within `tolerance`.
fn assert_stripes(rgba: &[u8], w: usize, h: usize, tolerance: i32) {
    for (i, &want) in STRIPES.iter().enumerate() {
        let x = (2 * i + 1) * w / (2 * STRIPES.len());
        for y in [h / 4, h / 2, 3 * h / 4] {
            let got = pixel(rgba, w, x, y);
            let close = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs() <= tolerance;
            assert!(
                close(got.0, want.0) && close(got.1, want.1) && close(got.2, want.2),
                "stripe {i} at ({x},{y}): got {got:?}, want {want:?}"
            );
        }
    }
}

#[test]
fn cpu_nv12_becomes_the_right_rgb() {
    let Some(mut r) = renderer() else { return };
    let (w, h) = (640, 360);
    let data = stripes_nv12(w, h);
    let pic = Picture::Nv12 {
        width: w as u32,
        height: h as u32,
        data: &data,
    };
    let rgba = r.render_to_rgba(Some(&pic), w as u32, h as u32).unwrap();
    assert_stripes(&rgba, w, h, 3);
    // Top-left is the first stripe: no flip in either direction.
    assert!(pixel(&rgba, w, 2, 2).0 > 200, "red must be top left");
}

#[test]
fn other_aspect_ratios_get_black_bars() {
    let Some(mut r) = renderer() else { return };
    let data = stripes_nv12(640, 360);
    let pic = Picture::Nv12 {
        width: 640,
        height: 360,
        data: &data,
    };
    // 16:9 in a square: picture 400×225 in the middle, bars above and below.
    let rgba = r.render_to_rgba(Some(&pic), 400, 400).unwrap();
    assert_eq!(pixel(&rgba, 400, 200, 20), (0, 0, 0), "bar on top");
    assert_eq!(pixel(&rgba, 400, 200, 380), (0, 0, 0), "bar at the bottom");
    assert_ne!(
        pixel(&rgba, 400, 200, 200),
        (0, 0, 0),
        "picture in the middle"
    );
    // Scaled down 2×, the stripes still have their colours.
    let rgba = r.render_to_rgba(Some(&pic), 320, 180).unwrap();
    assert_stripes(&rgba, 320, 180, 3);
}

#[test]
fn size_changes_and_no_picture() {
    let Some(mut r) = renderer() else { return };
    for (w, h) in [(320usize, 180usize), (1280, 720), (320, 180)] {
        let data = stripes_nv12(w, h);
        let pic = Picture::Nv12 {
            width: w as u32,
            height: h as u32,
            data: &data,
        };
        let rgba = r.render_to_rgba(Some(&pic), w as u32, h as u32).unwrap();
        assert_stripes(&rgba, w, h, 3);
    }
    // Synthetic streams have no picture: a black frame.
    let rgba = r.render_to_rgba(None, 64, 64).unwrap();
    assert!(rgba.chunks(4).all(|p| p == [0, 0, 0, 255]));
}

#[test]
fn bad_pictures_are_errors_not_crashes() {
    let Some(mut r) = renderer() else { return };
    let short = Picture::Nv12 {
        width: 64,
        height: 64,
        data: &[0; 10],
    };
    assert!(r.render_to_rgba(Some(&short), 64, 64).is_err());
    let odd = Picture::Nv12 {
        width: 63,
        height: 64,
        data: &[0; 63 * 96],
    };
    assert!(r.render_to_rgba(Some(&odd), 64, 64).is_err());
    let not_nv12 = fernsicht_capture::DmaBuf {
        width: 64,
        height: 64,
        fourcc: fernsicht_capture::dmabuf::formats::XRGB8888,
        modifier: 0,
        objects: vec![Arc::new(std::fs::File::open("/dev/null").unwrap().into())],
        planes: vec![fernsicht_capture::DmaBufPlane {
            object: 0,
            offset: 0,
            pitch: 256,
        }],
    };
    let e = r
        .render_to_rgba(
            Some(&Picture::DmaBuf {
                image: &not_nv12,
                key: 1,
            }),
            64,
            64,
        )
        .unwrap_err();
    assert!(
        matches!(e, fernsicht_render::vulkan::RenderError::Import(_)),
        "{e}"
    );
    // Still usable afterwards.
    let data = stripes_nv12(64, 64);
    let ok = Picture::Nv12 {
        width: 64,
        height: 64,
        data: &data,
    };
    r.render_to_rgba(Some(&ok), 64, 64).unwrap();
}

#[cfg(feature = "vaapi")]
mod vaapi {
    use std::time::Instant;

    use fernsicht_capture::{Frame, PixelFormat};
    use fernsicht_codec::vaapi::{
        DEFAULT_RENDER_NODE, VaapiDecoder, VaapiEncoder, VaapiEncoderConfig,
    };
    use fernsicht_codec::{DecodedFrame, Decoder, EncodedFrame, Encoder, PictureKind};

    use super::*;

    fn render_node() -> Option<String> {
        match std::env::var("FERNSICHT_GPU").as_deref() {
            Ok("amd" | "intel") => {
                Some(std::env::var("FERNSICHT_RENDER_NODE").unwrap_or(DEFAULT_RENDER_NODE.into()))
            }
            _ => {
                eprintln!("skipped: set FERNSICHT_GPU=amd|intel");
                None
            }
        }
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    }

    #[test]
    fn decoder_dmabuf_is_shown_without_a_copy() {
        let Some(node) = render_node() else { return };
        let Some(mut r) = renderer() else { return };
        assert!(
            r.gpu().can_import_dmabuf(),
            "{} lacks DMA-BUF import",
            r.gpu().name()
        );
        let (w, h) = (1920u32, 1080u32);
        let mut enc = VaapiEncoder::new(&VaapiEncoderConfig {
            render_node: node.clone(),
            width: w,
            height: h,
            fps: 60,
            bitrate_kbps: 20_000,
        })
        .unwrap();
        let mut dec = VaapiDecoder::new(&node).unwrap();
        let mut frame = Frame::new(w, h, PixelFormat::Nv12);
        frame.data = stripes_nv12(w as usize, h as usize);
        let (mut out, mut d) = (EncodedFrame::default(), DecodedFrame::default());
        let (mut zero_copy_ms, mut download_ms) = (Vec::new(), Vec::new());
        // Rendering into a small target measures import + conversion +
        // draw without the cost of reading 8 MB back for the comparison.
        let (mut via_dmabuf, mut via_cpu) = (Vec::new(), Vec::new());
        for i in 0..60u64 {
            frame.seq = i;
            enc.encode(&frame, &mut out).unwrap();
            dec.decode(&out.data, &mut d).unwrap();

            let t = Instant::now();
            let pic = dec.picture(PictureKind::DmaBuf).unwrap().unwrap();
            assert!(matches!(pic, Picture::DmaBuf { .. }));
            r.render_to_rgba(Some(&pic), 16, 16).unwrap();
            zero_copy_ms.push(t.elapsed().as_secs_f64() * 1e3);
            if i == 59 {
                via_dmabuf = r.render_to_rgba(Some(&pic), w, h).unwrap();
            }

            let t = Instant::now();
            let pic = dec.picture(PictureKind::Nv12).unwrap().unwrap();
            r.render_to_rgba(Some(&pic), 16, 16).unwrap();
            download_ms.push(t.elapsed().as_secs_f64() * 1e3);
            if i == 59 {
                via_cpu = r.render_to_rgba(Some(&pic), w, h).unwrap();
            }
        }
        // Both ways show the stripes (encoding costs a little accuracy).
        assert_stripes(&via_dmabuf, w as usize, h as usize, 6);
        assert_stripes(&via_cpu, w as usize, h as usize, 6);
        let differ = via_dmabuf
            .iter()
            .zip(&via_cpu)
            .filter(|(a, b)| (i32::from(**a) - i32::from(**b)).abs() > 2)
            .count();
        assert!(
            differ < via_cpu.len() / 1000,
            "{differ} bytes differ between the two paths"
        );

        let summary = format!(
            "### Vulkan-Anzeige des VAAPI-Decoders, 1080p ({})\n\
             | Weg | Median: Bild holen, umrechnen, zeichnen |\n|---|---|\n\
             | DMA-BUF, ohne Kopie | {:.2} ms |\n\
             | Download über die CPU | {:.2} ms |\n",
            r.gpu().name(),
            median(zero_copy_ms.clone()),
            median(download_ms.clone()),
        );
        println!("{summary}");
        if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            writeln!(f, "{summary}").unwrap();
        }
        assert!(
            median(zero_copy_ms) < median(download_ms),
            "zero copy should beat the download"
        );
    }
}
