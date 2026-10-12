//! RGB DMA-BUF → NV12 on the real GPU.
//!
//! Without Vulkan the tests skip, unless `FERNSICHT_REQUIRE_VULKAN` is set.
//! Without DMA-BUF import (llvmpipe in containers and on CI machines has
//! none) they skip, unless `FERNSICHT_REQUIRE_DMABUF` is set, as on the GPU
//! runners.

use std::sync::Arc;

use fernsicht_capture::dmabuf::{formats, fourcc_name};
use fernsicht_core::colour::{Colour, Depth};
use fernsicht_gpu::Gpu;
use fernsicht_gpu::convert::Converter;
use fernsicht_gpu::testimage::upload_as_dmabuf;

fn gpu() -> Option<Arc<Gpu>> {
    let required = |var: &str| std::env::var_os(var).is_some();
    match Gpu::new() {
        Ok(g) if g.can_import_dmabuf() => Some(Arc::new(g)),
        Ok(g) => {
            let why = format!("{} has no DMA-BUF import", g.name());
            assert!(
                !required("FERNSICHT_REQUIRE_DMABUF"),
                "DMA-BUF import required: {why}"
            );
            eprintln!("skipped: {why}");
            None
        }
        Err(e) => {
            assert!(
                !required("FERNSICHT_REQUIRE_VULKAN"),
                "Vulkan required: {e}"
            );
            eprintln!("skipped: {e}");
            None
        }
    }
}

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

/// Colour patches across the top half, a grey ramp below.
fn picture(w: usize, h: usize, format: u32) -> Vec<u8> {
    let mut img = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = if y < h / 2 {
                PATCHES[x * PATCHES.len() / w]
            } else {
                let v = (x * 255 / w) as u8;
                (v, v, v)
            };
            img[(y * w + x) * 4..][..4].copy_from_slice(&pack(format, r, g, b));
        }
    }
    img
}

/// What a patch becomes in `colour` (its depth's limited range and its
/// matrix), in code values of that depth.
fn expected(r: u8, g: u8, b: u8, colour: Colour) -> (f64, f64, f64) {
    let (r, g, b) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let (kr, kb) = colour.kr_kb();
    let (kr, kb) = (f64::from(kr), f64::from(kb));
    let (black, span, mid, cspan) = colour.depth.limited_range();
    let (black, span, mid, cspan) = (
        f64::from(black),
        f64::from(span),
        f64::from(mid),
        f64::from(cspan),
    );
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    (
        black + span * y,
        mid + cspan * (b - y) / (2.0 * (1.0 - kb)),
        mid + cspan * (r - y) / (2.0 * (1.0 - kr)),
    )
}

/// Sample `i` of a tightly packed NV12 (a byte) or P010 (a little-endian
/// word, the 10 bits on top) picture.
fn sample(buf: &[u8], depth: Depth, i: usize) -> f64 {
    match depth {
        Depth::Eight => f64::from(buf[i]),
        Depth::Ten => f64::from(u16::from_le_bytes([buf[2 * i], buf[2 * i + 1]]) >> 6),
    }
}

/// Mean Y, Cb, Cr in the middle of each patch, against `colour`. Exact
/// conversion, so tight bounds (10 bit sources round a little apart).
fn check_patches_in(yuv: &[u8], w: usize, h: usize, colour: Colour, what: &str) {
    let tolerance = 1.5 * f64::from(1u32 << (colour.depth.bits() - 8));
    let pw = w / PATCHES.len();
    for (i, &(r, g, b)) in PATCHES.iter().enumerate() {
        let (x0, x1) = (i * pw + pw / 4, (i + 1) * pw - pw / 4);
        let (y0, y1) = (h / 8, h * 3 / 8);
        let (mut ys, mut cbs, mut crs, mut n, mut nc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for y in y0..y1 {
            for x in x0..x1 {
                ys += sample(yuv, colour.depth, y * w + x);
                n += 1.0;
                if y % 2 == 0 && x % 2 == 0 {
                    let uv = w * h + (y / 2) * w + x;
                    cbs += sample(yuv, colour.depth, uv);
                    crs += sample(yuv, colour.depth, uv + 1);
                    nc += 1.0;
                }
            }
        }
        let got = (ys / n, cbs / nc, crs / nc);
        let want = expected(r, g, b, colour);
        assert!(
            (got.0 - want.0).abs() < tolerance
                && (got.1 - want.1).abs() < tolerance
                && (got.2 - want.2).abs() < tolerance,
            "{what}, patch {i} RGB({r},{g},{b}): got YCbCr {got:.1?}, {colour:?} wants {want:.1?}"
        );
    }
}

fn check_patches(nv12: &[u8], w: usize, h: usize, what: &str) {
    check_patches_in(nv12, w, h, Colour::SDR, what);
}

#[test]
fn every_desktop_format_converts_to_bt709() {
    let Some(gpu) = gpu() else { return };
    let (w, h) = (1280usize, 720usize);
    let mut conv = Converter::new(gpu.clone(), w as u32, h as u32, Colour::SDR).unwrap();
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
        let image = upload_as_dmabuf(&gpu, w as u32, h as u32, format, &picture(w, h, format))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        conv.convert(&image)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let nv12 = conv.download().unwrap();
        assert_eq!(nv12.len(), w * h * 3 / 2);
        check_patches(
            &nv12,
            w,
            h,
            &format!("{name}, modifier {:#x}", image.modifier),
        );
        eprintln!("{name} (modifier {:#x}): ok", image.modifier);
    }
}

#[test]
fn ten_bit_desktops_convert_to_p010_in_either_matrix() {
    let Some(gpu) = gpu() else { return };
    let (w, h) = (1280usize, 720usize);
    let ten_bit_sdr = Colour::SDR.with_depth(Depth::Ten);
    for colour in [ten_bit_sdr, Colour::HDR10] {
        let mut conv = Converter::new(gpu.clone(), w as u32, h as u32, colour).unwrap();
        for format in [
            formats::XRGB2101010,
            formats::ABGR2101010,
            formats::XRGB8888,
        ] {
            let name = fourcc_name(format);
            let image = upload_as_dmabuf(&gpu, w as u32, h as u32, format, &picture(w, h, format))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            conv.convert(&image)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let p010 = conv.download().unwrap();
            assert_eq!(p010.len(), w * h * 3, "two bytes a sample");
            check_patches_in(&p010, w, h, colour, &format!("{name} → {colour:?}"));
        }
    }
}

#[test]
fn chroma_is_sited_left() {
    let Some(gpu) = gpu() else { return };
    let (w, h) = (16usize, 2usize);
    // Grey with one red column at an odd x (5).
    let mut img = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = if x == 5 { (255, 0, 0) } else { (128, 128, 128) };
            img[(y * w + x) * 4..][..4].copy_from_slice(&pack(formats::XRGB8888, r, g, b));
        }
    }
    let mut conv = Converter::new(gpu.clone(), w as u32, h as u32, Colour::SDR).unwrap();
    let image = upload_as_dmabuf(&gpu, w as u32, h as u32, formats::XRGB8888, &img).unwrap();
    conv.convert(&image).unwrap();
    let nv12 = conv.download().unwrap();
    let cr = |k: usize| i32::from(nv12[w * h + 2 * k + 1]);
    // Left siting: chroma k sits on column 2k, so column 5 lies between
    // chroma 2 and 3 and counts the same for both. (Centred siting would
    // give it all to chroma 2.)
    assert!(cr(2) > 140, "red shows: Cr {}", cr(2));
    assert!((cr(2) - cr(3)).abs() <= 2, "Cr {} and {}", cr(2), cr(3));
    assert!((cr(0) - 128).abs() <= 2 && (cr(5) - 128).abs() <= 2);
}

#[test]
fn pictures_are_scaled_to_the_output_size() {
    let Some(gpu) = gpu() else { return };
    let src = upload_as_dmabuf(
        &gpu,
        2560,
        1440,
        formats::XRGB8888,
        &picture(2560, 1440, formats::XRGB8888),
    )
    .unwrap();
    for (w, h) in [(1920usize, 1080usize), (1366, 768), (640, 360)] {
        let mut conv = Converter::new(gpu.clone(), w as u32, h as u32, Colour::SDR).unwrap();
        conv.convert(&src).unwrap();
        check_patches(&conv.download().unwrap(), w, h, &format!("{w}×{h}"));
    }
}

#[test]
fn the_output_memory_can_be_exported() {
    let Some(gpu) = gpu() else { return };
    let conv = Converter::new(gpu, 1920, 1080, Colour::SDR).unwrap();
    assert!(conv.allocation_size() >= conv.layout().len());
    let a = conv.export_output().unwrap();
    let b = conv.export_output().unwrap();
    use std::os::fd::AsRawFd;
    assert_ne!(
        a.as_raw_fd(),
        b.as_raw_fd(),
        "each export is its own descriptor"
    );
}

#[test]
fn bad_input_is_an_error_and_the_converter_keeps_working() {
    let Some(gpu) = gpu() else { return };
    let mut conv = Converter::new(gpu.clone(), 640, 360, Colour::SDR).unwrap();
    let good = upload_as_dmabuf(
        &gpu,
        640,
        360,
        formats::XRGB8888,
        &picture(640, 360, formats::XRGB8888),
    )
    .unwrap();
    let yuyv = fernsicht_capture::DmaBuf {
        fourcc: fernsicht_capture::dmabuf::fourcc(b"YUYV"),
        ..good.clone()
    };
    assert!(conv.convert(&yuyv).is_err());
    let not_a_dmabuf = fernsicht_capture::DmaBuf {
        objects: vec![Arc::new(std::fs::File::open("/dev/null").unwrap().into())],
        ..good.clone()
    };
    assert!(conv.convert(&not_a_dmabuf).is_err());
    conv.convert(&good).unwrap();
    check_patches(&conv.download().unwrap(), 640, 360, "after errors");
    assert!(Converter::new(gpu, 641, 360, Colour::SDR).is_err());
}

#[test]
fn buffers_are_imported_once_even_with_new_descriptors() {
    let Some(gpu) = gpu() else { return };
    let mut conv = Converter::new(gpu.clone(), 640, 360, Colour::SDR).unwrap();
    let pics: Vec<_> = (0..6)
        .map(|_| {
            upload_as_dmabuf(
                &gpu,
                640,
                360,
                formats::XRGB8888,
                &picture(640, 360, formats::XRGB8888),
            )
            .unwrap()
        })
        .collect();
    // Like KMS: the same two buffers, a fresh descriptor every frame.
    for i in 0..10 {
        let mut img = pics[i % 2].clone();
        img.objects = vec![Arc::new(img.objects[0].try_clone().unwrap())];
        conv.convert(&img).unwrap();
    }
    assert_eq!(conv.cached_imports(), 2);
    // More buffers than the cache holds: the oldest go.
    for p in &pics {
        conv.convert(p).unwrap();
    }
    assert_eq!(conv.cached_imports(), 4);
    check_patches(&conv.download().unwrap(), 640, 360, "after cache churn");
}
