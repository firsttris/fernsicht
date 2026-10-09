use fernsicht_capture::{Frame, PixelFormat};
use fernsicht_codec::synthetic::{SyntheticDecoder, SyntheticEncoder};
use fernsicht_codec::{CodecError, DecodedFrame, Decoder, EncodedFrame, Encoder};
use proptest::prelude::*;

fn encode(bitrate: u32, fps: u32, seq: u64, keyframe: bool) -> EncodedFrame {
    let mut enc = SyntheticEncoder::new(bitrate, fps);
    let mut out = EncodedFrame::default();
    let mut frame = Frame::new(64, 32, PixelFormat::Nv12);
    if !keyframe {
        enc.encode(&frame, &mut out).unwrap();
    }
    frame.seq = seq;
    enc.encode(&frame, &mut out).unwrap();
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn roundtrip(bitrate in 100u32..100_000, fps in 1u32..240, seq: u64) {
        let out = encode(bitrate, fps, seq, true);
        let base = (bitrate as usize * 1000 / 8) / fps as usize;
        prop_assert!(out.data.len() >= base.min(21 + 16));
        let mut d = DecodedFrame::default();
        SyntheticDecoder::default().decode(&out.data, &mut d).unwrap();
        prop_assert_eq!(d, DecodedFrame { width: 64, height: 32, seq, keyframe: true });
    }

    /// Any single corrupted byte is detected.
    #[test]
    fn single_byte_corruption_detected(seq: u64, pos: prop::sample::Index, flip in 1u8..) {
        let mut out = encode(2_000, 60, seq, true);
        let i = pos.index(out.data.len());
        out.data[i] ^= flip;
        let mut d = DecodedFrame::default();
        let r = SyntheticDecoder::default().decode(&out.data, &mut d);
        prop_assert!(matches!(r, Err(CodecError::Corrupt(_))), "byte {} undetected", i);
    }

    #[test]
    fn truncation_detected(seq: u64, keep: prop::sample::Index) {
        let out = encode(2_000, 60, seq, true);
        let n = keep.index(out.data.len());
        let mut d = DecodedFrame::default();
        prop_assert!(SyntheticDecoder::default().decode(&out.data[..n], &mut d).is_err());
    }
}
