//! Synthetic codec: realistic frame sizes, verifiable payload.
//!
//! Bitstream: `"FSYN" | seq u64 | width u16 | height u16 | keyframe u8 |
//! checksum u32 | body`. The body is pseudo-random (incompressible, like
//! real encoder output) and covered by an FNV-1a checksum, so any
//! transport corruption is detected by the decoder.

use fernsicht_capture::Frame;
use fernsicht_core::now_us;
use fernsicht_proto::Codec;

use crate::{CodecError, DecodedFrame, Decoder, EncodedFrame, Encoder};

const MAGIC: &[u8; 4] = b"FSYN";
const HEADER: usize = 4 + 8 + 2 + 2 + 1 + 4;
/// Keyframes are this much larger than delta frames.
const KEYFRAME_FACTOR: usize = 6;

pub struct SyntheticEncoder {
    bitrate_kbps: u32,
    fps: u32,
    force_keyframe: bool,
}

impl SyntheticEncoder {
    pub fn new(bitrate_kbps: u32, fps: u32) -> Self {
        Self {
            bitrate_kbps,
            fps: fps.max(1),
            force_keyframe: true,
        }
    }

    fn frame_bytes(&self, keyframe: bool, seq: u64) -> usize {
        let base = (self.bitrate_kbps as usize * 1000 / 8) / self.fps as usize;
        // ±12.5 % size variation, like a real encoder on changing content.
        let jitter = base / 8;
        let size = base - jitter + mix(seq) as usize % (2 * jitter + 1);
        let size = if keyframe {
            size * KEYFRAME_FACTOR
        } else {
            size
        };
        size.max(HEADER + 16)
    }
}

impl Encoder for SyntheticEncoder {
    fn codec(&self) -> Codec {
        Codec::Synthetic
    }

    fn encode(&mut self, frame: &Frame, out: &mut EncodedFrame) -> Result<(), CodecError> {
        let keyframe = std::mem::take(&mut self.force_keyframe);
        let len = self.frame_bytes(keyframe, frame.seq);
        out.data.resize(len, 0);
        let (head, body) = out.data.split_at_mut(HEADER);
        let mut state = mix(frame.seq) | 1;
        for chunk in body.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
        }
        head[..4].copy_from_slice(MAGIC);
        head[4..12].copy_from_slice(&frame.seq.to_le_bytes());
        head[12..14].copy_from_slice(&(frame.width as u16).to_le_bytes());
        head[14..16].copy_from_slice(&(frame.height as u16).to_le_bytes());
        head[16] = u8::from(keyframe);
        head[17..21].copy_from_slice(&fnv1a(body).to_le_bytes());

        out.keyframe = keyframe;
        out.seq = frame.seq;
        out.capture_us = frame.capture_us;
        out.capture_ready_us = frame.ready_us;
        out.encoded_us = now_us();
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.bitrate_kbps = kbps.max(100);
    }
}

#[derive(Default)]
pub struct SyntheticDecoder {
    have_keyframe: bool,
}

impl Decoder for SyntheticDecoder {
    fn decode(&mut self, data: &[u8], out: &mut DecodedFrame) -> Result<(), CodecError> {
        if data.len() < HEADER {
            return Err(CodecError::Corrupt("short frame"));
        }
        let (head, body) = data.split_at(HEADER);
        if &head[..4] != MAGIC {
            return Err(CodecError::Corrupt("bad magic"));
        }
        let checksum = u32::from_le_bytes(head[17..21].try_into().unwrap());
        if fnv1a(body) != checksum {
            return Err(CodecError::Corrupt("checksum mismatch"));
        }
        let keyframe = head[16] != 0;
        if keyframe {
            self.have_keyframe = true;
        } else if !self.have_keyframe {
            return Err(CodecError::NeedKeyframe);
        }
        *out = DecodedFrame {
            seq: u64::from_le_bytes(head[4..12].try_into().unwrap()),
            width: u32::from(u16::from_le_bytes(head[12..14].try_into().unwrap())),
            height: u32::from(u16::from_le_bytes(head[14..16].try_into().unwrap())),
            keyframe,
        };
        Ok(())
    }
}

fn mix(seq: u64) -> u64 {
    // splitmix64 finalizer
    let mut z = seq.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn fnv1a(data: &[u8]) -> u32 {
    let mut h = 0x811C_9DC5u32;
    for &b in data {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use fernsicht_capture::PixelFormat;

    fn frame(seq: u64) -> Frame {
        let mut f = Frame::new(1920, 1080, PixelFormat::Nv12);
        f.seq = seq;
        f
    }

    #[test]
    fn roundtrip_and_sizes() {
        let mut enc = SyntheticEncoder::new(20_000, 60);
        let mut dec = SyntheticDecoder::default();
        let mut out = EncodedFrame::default();
        let mut decoded = DecodedFrame::default();

        enc.encode(&frame(0), &mut out).unwrap();
        assert!(out.keyframe);
        let key_len = out.data.len();
        dec.decode(&out.data, &mut decoded).unwrap();
        assert_eq!(
            (decoded.seq, decoded.width, decoded.height),
            (0, 1920, 1080)
        );

        enc.encode(&frame(1), &mut out).unwrap();
        assert!(!out.keyframe);
        // 20 Mbit/s at 60 fps ≈ 41.7 kB per frame (±12.5 %)
        assert!(
            (36_000..48_000).contains(&out.data.len()),
            "{}",
            out.data.len()
        );
        assert!(key_len > 4 * out.data.len());
        dec.decode(&out.data, &mut decoded).unwrap();
        assert_eq!(decoded.seq, 1);

        enc.request_keyframe();
        enc.encode(&frame(2), &mut out).unwrap();
        assert!(out.keyframe);
    }

    #[test]
    fn detects_corruption() {
        let mut enc = SyntheticEncoder::new(5_000, 60);
        let mut out = EncodedFrame::default();
        enc.encode(&frame(7), &mut out).unwrap();
        let mid = out.data.len() / 2;
        out.data[mid] ^= 0xFF;
        let mut dec = SyntheticDecoder::default();
        let mut d = DecodedFrame::default();
        assert!(matches!(
            dec.decode(&out.data, &mut d),
            Err(CodecError::Corrupt(_))
        ));
    }

    #[test]
    fn delta_before_keyframe_is_rejected() {
        let mut enc = SyntheticEncoder::new(5_000, 60);
        let mut out = EncodedFrame::default();
        enc.encode(&frame(0), &mut out).unwrap(); // keyframe, discarded
        enc.encode(&frame(1), &mut out).unwrap();
        let mut dec = SyntheticDecoder::default();
        let mut d = DecodedFrame::default();
        assert!(matches!(
            dec.decode(&out.data, &mut d),
            Err(CodecError::NeedKeyframe)
        ));
    }
}
