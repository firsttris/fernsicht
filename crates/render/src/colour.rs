//! What the fragment shader needs to turn a picture's samples into RGB
//! (D4): the code value scale of its depth, the limited range at that
//! depth and the YCbCr → RGB matrix of its primaries; and for HDR (PQ)
//! pictures on an SDR display the tone mapping, which [`pq_to_sdr`]
//! spells out (the shader does the same, `shader.wgsl`).

use fernsicht_core::colour::{Colour, Depth, Transfer};

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
        transfer_code(colour.transfer),
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

/// The shader's transfer modes: 0 shows the values as they are (SDR), 1
/// tone-maps PQ to SDR ([`pq_to_sdr`]).
pub fn transfer_code(transfer: Transfer) -> u32 {
    match transfer {
        Transfer::Sdr => 0,
        Transfer::Pq => 1,
    }
}

/// Nits of SDR white in an HDR signal (ITU-R BT.2408's reference white):
/// what an SDR display shows as white.
pub const SDR_WHITE_NITS: f32 = 203.0;

/// The brightest an HDR picture is taken to be, nits (no mastering
/// metadata travels with the stream; HDR desktops master for about this).
pub const HDR_PEAK_NITS: f32 = 1000.0;

/// Below this (relative to SDR white) the tone curve leaves values alone;
/// above, it compresses up to [`HDR_PEAK_NITS`] into what is left.
pub const KNEE: f32 = 0.8;

/// SMPTE ST 2084's EOTF: a PQ value (0..1) as nits.
pub fn pq_to_nits(e: f32) -> f32 {
    let (m1, m2) = (0.159_301_76, 78.843_75);
    let (c1, c2, c3) = (0.835_937_5, 18.851_563, 18.6875);
    let p = e.max(0.0).powf(1.0 / m2);
    10_000.0 * ((p - c1).max(0.0) / (c2 - c3 * p)).powf(1.0 / m1)
}

/// Its inverse: nits as a PQ value.
pub fn nits_to_pq(nits: f32) -> f32 {
    let (m1, m2) = (0.159_301_76, 78.843_75);
    let (c1, c2, c3) = (0.835_937_5, 18.851_563, 18.6875);
    let y = (nits / 10_000.0).clamp(0.0, 1.0).powf(m1);
    ((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
}

/// The highlights' curve, on light relative to SDR white: the identity
/// up to [`KNEE`], then a rational curve with the same slope there that
/// reaches 1 at the peak.
pub fn tone(x: f32) -> f32 {
    let peak = HDR_PEAK_NITS / SDR_WHITE_NITS;
    if x <= KNEE {
        return x;
    }
    // g(t) = (1 + c)·t / (t + c) on t in 0..1, slope 1 at the knee.
    let c = (1.0 - KNEE) / (peak - 1.0);
    let t = ((x - KNEE) / (peak - KNEE)).min(1.0);
    KNEE + (1.0 - KNEE) * (1.0 + c) * t / (t + c)
}

/// Linear BT.2020 RGB to linear BT.709 RGB (ITU-R BT.2087).
pub const BT2020_TO_BT709: [[f32; 3]; 3] = [
    [1.6605, -0.5876, -0.0728],
    [-0.1246, 1.1329, -0.0083],
    [-0.0182, -0.1006, 1.1187],
];

/// The sRGB curve on linear light (0..1).
pub fn srgb_encode(l: f32) -> f32 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// An HDR10 pixel (PQ-encoded BT.2020 RGB, 0..1) as an SDR display shows
/// it (sRGB-encoded BT.709 RGB, 0..1): light relative to SDR white, the
/// primaries converted (what lies outside BT.709 clipped), highlights
/// toned by the brightest channel, so that hues keep.
pub fn pq_to_sdr(rgb: [f32; 3]) -> [f32; 3] {
    let lin = rgb.map(|e| pq_to_nits(e) / SDR_WHITE_NITS);
    let m = BT2020_TO_BT709;
    let narrow =
        [0, 1, 2].map(|i| (m[i][0] * lin[0] + m[i][1] * lin[1] + m[i][2] * lin[2]).max(0.0));
    let peak = narrow.iter().copied().fold(0.0f32, f32::max);
    let scale = if peak > 0.0 { tone(peak) / peak } else { 1.0 };
    narrow.map(|v| srgb_encode(v * scale))
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
    fn pq_round_trips_and_has_its_landmarks() {
        for nits in [0.0, 0.1, 100.0, 203.0, 1000.0, 10_000.0] {
            let back = pq_to_nits(nits_to_pq(nits));
            assert!((back - nits).abs() <= nits * 1e-3 + 1e-3, "{nits} → {back}");
        }
        // 100 nits is about 0.508, 1000 nits about 0.752 (ST 2084).
        assert!((nits_to_pq(100.0) - 0.508).abs() < 1e-3);
        assert!((nits_to_pq(1000.0) - 0.752).abs() < 1e-3);
    }

    #[test]
    fn the_tone_curve_keeps_the_shadows_and_ends_at_the_peak() {
        assert_eq!(tone(0.0), 0.0);
        assert_eq!(tone(0.5), 0.5, "below the knee: untouched");
        let peak = HDR_PEAK_NITS / SDR_WHITE_NITS;
        assert!((tone(peak) - 1.0).abs() < 1e-5);
        assert!(tone(peak * 4.0) <= 1.0, "beyond the peak: clipped");
        // Continuous and rising, the slope 1 at the knee.
        let d = 1e-3;
        assert!(((tone(KNEE + d) - tone(KNEE)) / d - 1.0).abs() < 0.01);
        let mut last = 0.0;
        for i in 0..1000 {
            let v = tone(i as f32 * peak / 999.0);
            assert!(v >= last);
            last = v;
        }
    }

    #[test]
    fn hdr_greys_show_as_sdr_greys() {
        let grey = |nits: f32| pq_to_sdr([nits_to_pq(nits); 3]);
        assert_eq!(grey(0.0), [0.0; 3]);
        // 100 nits is half SDR white: below the knee, sRGB of 100/203.
        let half = grey(100.0);
        assert!((half[0] - srgb_encode(100.0 / 203.0)).abs() < 1e-3);
        let spread = half.iter().fold(0.0f32, |m, v| m.max((v - half[0]).abs()));
        assert!(spread < 1e-3, "grey stays grey: {half:?}");
        // SDR white sits a little below white, the peak at white.
        assert!((0.9..1.0).contains(&grey(203.0)[0]));
        assert!(grey(1000.0)[0] > 0.999);
        // Pure BT.2020 red is out of BT.709's gamut: clipped to red.
        let red = pq_to_sdr([nits_to_pq(100.0), 0.0, 0.0]);
        assert!(red[0] > 0.8 && red[1] == 0.0 && red[2] == 0.0, "{red:?}");
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
        assert_eq!(u32::from_ne_bytes(p[28..32].try_into().unwrap()), 1, "PQ");
        let c = cursor_push([0.1, 0.2, 0.3, 0.4]);
        assert_eq!(f32::from_ne_bytes(c[16..20].try_into().unwrap()), 1.0);
    }
}
