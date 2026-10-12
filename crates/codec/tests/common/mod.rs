//! Shared by the codec crate's GPU tests: what colour patches become in a
//! stream's colour, read back from NV12 or P010.

#![allow(dead_code)]

use fernsicht_core::colour::{Colour, Depth};

/// What RGB (8 bit per channel) becomes in `colour`: its depth's limited
/// range and its matrix, in code values of that depth.
pub fn expected(r: u8, g: u8, b: u8, colour: Colour) -> (f64, f64, f64) {
    let unit = |v: u8| f64::from(v) / 255.0;
    let (r, g, b) = (unit(r), unit(g), unit(b));
    let (kr, kb) = colour.kr_kb();
    let (kr, kb) = (f64::from(kr), f64::from(kb));
    let (black, span, mid, cspan) = colour.depth.limited_range();
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    (
        f64::from(black) + f64::from(span) * y,
        f64::from(mid) + f64::from(cspan) * (b - y) / (2.0 * (1.0 - kb)),
        f64::from(mid) + f64::from(cspan) * (r - y) / (2.0 * (1.0 - kr)),
    )
}

/// Sample `i` of tightly packed NV12 (a byte) or P010 (a little-endian
/// word, the 10 bits on top).
pub fn sample(buf: &[u8], depth: Depth, i: usize) -> f64 {
    match depth {
        Depth::Eight => f64::from(buf[i]),
        Depth::Ten => f64::from(u16::from_le_bytes([buf[2 * i], buf[2 * i + 1]]) >> 6),
    }
}

/// Mean Y, Cb, Cr in the middle of each of `patches` (side by side across
/// the top half) against `colour`, within `tolerance` 8-bit codes (scaled
/// to the depth): room for compression.
pub fn assert_patches(
    yuv: &[u8],
    (w, h): (usize, usize),
    patches: &[(u8, u8, u8)],
    colour: Colour,
    tolerance: f64,
    what: &str,
) {
    let tolerance = tolerance * f64::from(1u32 << (colour.depth.bits() - 8));
    let pw = w / patches.len();
    for (i, &(r, g, b)) in patches.iter().enumerate() {
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
                && (got.1 - want.1).abs() < tolerance * 1.25
                && (got.2 - want.2).abs() < tolerance * 1.25,
            "{what}, patch {i} RGB({r},{g},{b}): got YCbCr {got:.1?}, {colour:?} wants {want:.1?}"
        );
    }
}

/// `patches` side by side across the top half of a `w`×`h` picture, a
/// grey ramp below, as XRGB2101010 (the 10-bit desktops' layout).
pub fn xr30_patches(w: usize, h: usize, patches: &[(u8, u8, u8)]) -> Vec<u8> {
    let ten = |v: u8| (u32::from(v) * 1023 + 127) / 255;
    let mut img = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = if y < h / 2 {
                patches[x * patches.len() / w]
            } else {
                let v = (x * 255 / w) as u8;
                (v, v, v)
            };
            let px = 3 << 30 | ten(r) << 20 | ten(g) << 10 | ten(b);
            img[(y * w + x) * 4..][..4].copy_from_slice(&px.to_le_bytes());
        }
    }
    img
}
