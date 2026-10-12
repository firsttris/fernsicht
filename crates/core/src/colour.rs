//! How a picture's values are to be read: bit depth, colour primaries and
//! transfer function (D4). Host and client agree on it through the
//! bitstream (the encoder writes it into the stream's VUI or sequence
//! header, the decoder reads it from the frame); this is the one type the
//! capture, the converter, the codecs and the renderer pass along.
//!
//! The matrix follows the primaries (BT.709, or BT.2020 non-constant
//! luminance) and the range is always limited ("TV"): what hardware
//! encoders and every player expect.

/// Bits per sample of the encoded YCbCr picture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Depth {
    #[default]
    Eight,
    /// P010: each sample in a 16-bit word, its 10 bits on top.
    Ten,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Primaries {
    /// sRGB's and HDTV's.
    #[default]
    Bt709,
    /// Wide gamut, what HDR desktops send their monitor.
    Bt2020,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Transfer {
    /// The SDR curve: sRGB on a desktop, tagged BT.709 in the stream
    /// (players show both alike).
    #[default]
    Sdr,
    /// SMPTE ST 2084 (PQ): HDR10, absolute luminance up to 10,000 nits.
    Pq,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Colour {
    pub depth: Depth,
    pub primaries: Primaries,
    pub transfer: Transfer,
}

impl Colour {
    /// What everything was before D4: 8 bit, BT.709, SDR.
    pub const SDR: Colour = Colour {
        depth: Depth::Eight,
        primaries: Primaries::Bt709,
        transfer: Transfer::Sdr,
    };

    /// HDR10: 10 bit, BT.2020, PQ.
    pub const HDR10: Colour = Colour {
        depth: Depth::Ten,
        primaries: Primaries::Bt2020,
        transfer: Transfer::Pq,
    };

    pub fn is_hdr(&self) -> bool {
        self.transfer == Transfer::Pq
    }

    pub fn with_depth(self, depth: Depth) -> Colour {
        Colour { depth, ..self }
    }

    /// The luma weights (Kr, Kb) of the YCbCr matrix.
    pub fn kr_kb(&self) -> (f32, f32) {
        match self.primaries {
            Primaries::Bt709 => (0.2126, 0.0722),
            // BT.2020 non-constant luminance.
            Primaries::Bt2020 => (0.2627, 0.0593),
        }
    }
}

impl Depth {
    pub fn bits(self) -> u32 {
        match self {
            Depth::Eight => 8,
            Depth::Ten => 10,
        }
    }

    /// Bytes one sample takes in memory (NV12: 1, P010: 2).
    pub fn sample_bytes(self) -> u32 {
        match self {
            Depth::Eight => 1,
            Depth::Ten => 2,
        }
    }

    /// Limited range at this depth: luma black and its span, the chroma
    /// midpoint and its span (BT.709 / BT.2020: 16, 219, 128, 224 scaled
    /// by 2^(bits − 8)).
    pub fn limited_range(self) -> (f32, f32, f32, f32) {
        let s = (1u32 << (self.bits() - 8)) as f32;
        (16.0 * s, 219.0 * s, 128.0 * s, 224.0 * s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_bit_limited_range_is_eight_bit_scaled() {
        assert_eq!(Depth::Eight.limited_range(), (16.0, 219.0, 128.0, 224.0));
        // BT.2100: black 64, white 940, chroma 512 ± 448.
        assert_eq!(Depth::Ten.limited_range(), (64.0, 876.0, 512.0, 896.0));
        assert_eq!(Depth::Ten.sample_bytes(), 2);
    }

    #[test]
    fn the_matrix_follows_the_primaries() {
        assert_eq!(Colour::SDR.kr_kb(), (0.2126, 0.0722));
        assert_eq!(Colour::HDR10.kr_kb(), (0.2627, 0.0593));
        assert!(Colour::HDR10.is_hdr() && !Colour::SDR.is_hdr());
        assert_eq!(
            Colour::SDR.with_depth(Depth::Ten).primaries,
            Primaries::Bt709
        );
    }
}
