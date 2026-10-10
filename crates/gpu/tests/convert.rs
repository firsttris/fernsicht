//! RGB DMA-BUF → NV12 on the real GPU.
//!
//! Without Vulkan the tests skip, unless `FERNSICHT_REQUIRE_VULKAN` is set.
//! Without DMA-BUF import (llvmpipe in containers and on CI machines has
//! none) they skip, unless `FERNSICHT_REQUIRE_DMABUF` is set, as on the GPU
//! runners.

use std::sync::Arc;

use fernsicht_capture::dmabuf::{formats, fourcc_name};
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

/// Mean Y, Cb, Cr in the middle of each patch, against BT.709. Exact
/// conversion, so tight bounds (10 bit sources round a little apart).
fn check_patches(nv12: &[u8], w: usize, h: usize, what: &str) {
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
            (got.0 - want.0).abs() < 1.5
                && (got.1 - want.1).abs() < 1.5
                && (got.2 - want.2).abs() < 1.5,
            "{what}, patch {i} RGB({r},{g},{b}): got YCbCr {got:.1?}, BT.709 wants {want:.1?}"
        );
    }
}

#[test]
fn every_desktop_format_converts_to_bt709() {
    let Some(gpu) = gpu() else { return };
    let (w, h) = (1280usize, 720usize);
    let mut conv = Converter::new(gpu.clone(), w as u32, h as u32).unwrap();
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
        let mut conv = Converter::new(gpu.clone(), w as u32, h as u32).unwrap();
        conv.convert(&src).unwrap();
        check_patches(&conv.download().unwrap(), w, h, &format!("{w}×{h}"));
    }
}

#[test]
fn the_output_memory_can_be_exported() {
    let Some(gpu) = gpu() else { return };
    let conv = Converter::new(gpu, 1920, 1080).unwrap();
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
    let mut conv = Converter::new(gpu.clone(), 640, 360).unwrap();
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
    assert!(Converter::new(gpu, 641, 360).is_err());
}

#[test]
fn buffers_are_imported_once_even_with_new_descriptors() {
    let Some(gpu) = gpu() else { return };
    let mut conv = Converter::new(gpu.clone(), 640, 360).unwrap();
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
