//! What the fragment shader needs to turn a picture's samples into RGB
//! (D4): the code value scale of its depth, the limited range at that
//! depth and the YCbCr → RGB matrix of its primaries.

use fernsicht_core::colour::{Colour, Depth};

/// Bytes of the push constants every draw sets (`Push` in `shader.wgsl`).
pub const PUSH_SIZE: u32 = 64;

/// R' = Y + a·Cr, G' = Y − b·Cb − c·Cr, B' = Y + d·Cb, for luma weights
/// (Kr, Kb) (BT.709: 1.5748, 0.1873, 0.4681, 1.8556).
pub fn ycbcr_to_rgb(kr: f32, kb: f32) -> [f32; 4] {
    let kg = 1.0 - kr - kb;
    [
        2.0 * (1.0 - kr),
        2.0 * kb * (1.0 - kb) / kg,
        2.0 * kr * (1.0 - kr) / kg,
        2.0 * (1.0 - kb),
    ]
}

/// What a sampled UNORM value is multiplied by to get the code value: 255
/// for bytes, and for 16-bit words with the 10 bits on top 65535 / 64.
pub fn code_scale(depth: Depth) -> f32 {
    match depth {
        Depth::Eight => 255.0,
        Depth::Ten => 65535.0 / 64.0,
    }
}

/// The push constants of a video draw: the quad's `scale` (and no
/// offset), the `crop` of the image shown, and `colour`'s parameters.
pub fn video_push(scale: [f32; 2], crop: [f32; 2], colour: &Colour) -> [u8; PUSH_SIZE as usize] {
    let (kr, kb) = colour.kr_kb();
    let (black, span, mid, cspan) = colour.depth.limited_range();
    let m = ycbcr_to_rgb(kr, kb);
    let words: [u32; 16] = [
        scale[0].to_bits(),
        scale[1].to_bits(),
        0f32.to_bits(),
        0f32.to_bits(),
        crop[0].to_bits(),
        crop[1].to_bits(),
        code_scale(colour.depth).to_bits(),
        // The transfer (0: SDR), for HDR.
        0,
        black.to_bits(),
        span.to_bits(),
        mid.to_bits(),
        cspan.to_bits(),
        m[0].to_bits(),
        m[1].to_bits(),
        m[2].to_bits(),
        m[3].to_bits(),
    ];
    pack(words)
}

/// The push constants of a pointer draw: its rectangle (scale, offset),
/// the whole image; the colour part is unused.
pub fn cursor_push(rect: [f32; 4]) -> [u8; PUSH_SIZE as usize] {
    let mut words = [0u32; 16];
    for (w, v) in words.iter_mut().zip(rect.iter().chain(&[1.0, 1.0])) {
        *w = v.to_bits();
    }
    pack(words)
}

fn pack(words: [u32; 16]) -> [u8; PUSH_SIZE as usize] {
    let mut out = [0u8; PUSH_SIZE as usize];
    for (chunk, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(words) {
        *chunk = w.to_ne_bytes();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn the_matrices_are_the_standards() {
        // BT.709 as the shader had it, BT.2020 as ITU-R BT.2020 gives it.
        assert!(close(
            ycbcr_to_rgb(0.2126, 0.0722),
            [1.5748, 0.1873, 0.4681, 1.8556]
        ));
        assert!(close(
            ycbcr_to_rgb(0.2627, 0.0593),
            [1.4746, 0.1646, 0.5714, 1.8814]
        ));
    }

    #[test]
    fn ten_bit_words_scale_to_their_codes() {
        // P010 white (940 << 6) sampled as UNORM16 reads back as 940.
        let sampled = f32::from(940u16 << 6) / 65535.0;
        assert!((sampled * code_scale(Depth::Ten) - 940.0).abs() < 1e-3);
        assert_eq!(code_scale(Depth::Eight), 255.0);
    }

    #[test]
    fn the_push_constants_carry_the_colour() {
        let p = video_push([0.5, 1.0], [1.0, 0.99], &Colour::HDR10);
        let word = |i: usize| f32::from_ne_bytes(p[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!([word(0), word(1), word(5)], [0.5, 1.0, 0.99]);
        assert_eq!(
            [word(8), word(9), word(10), word(11)],
            [64.0, 876.0, 512.0, 896.0]
        );
        assert!((word(12) - 1.4746).abs() < 1e-4, "the BT.2020 matrix");
        let c = cursor_push([0.1, 0.2, 0.3, 0.4]);
        assert_eq!(f32::from_ne_bytes(c[16..20].try_into().unwrap()), 1.0);
    }
}
