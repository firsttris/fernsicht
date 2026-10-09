//! Fernsicht wire format.
//!
//! Every datagram starts with a 4-byte prefix: magic, protocol version,
//! packet kind and flags. All integers are little-endian. Parsing never
//! panics and never allocates; malformed input yields a [`DecodeError`].

mod wire;

use thiserror::Error;
use wire::{Reader, Writer};

pub const MAGIC: u8 = 0xF5;
pub const VERSION: u8 = 1;
pub const PREFIX_LEN: usize = 4;

/// Largest datagram we ever send. Stays below a 1500-byte Ethernet MTU with
/// IPv6 + UDP headers and leaves room for the per-packet AEAD tag added in
/// phase 3.
pub const MAX_DATAGRAM: usize = 1400;

/// Default shard size: largest even payload that fits [`MAX_DATAGRAM`].
pub const DEFAULT_SHARD_SIZE: usize = (MAX_DATAGRAM - VideoHeader::LEN) & !1;

const _: () = assert!(DEFAULT_SHARD_SIZE % 2 == 0);
const _: () = assert!(VideoHeader::LEN + DEFAULT_SHARD_SIZE <= MAX_DATAGRAM);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Video = 1,
    Feedback = 2,
    ClockPing = 3,
    ClockPong = 4,
    Hello = 5,
    HelloAck = 6,
    Bye = 7,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Kind::Video,
            2 => Kind::Feedback,
            3 => Kind::ClockPing,
            4 => Kind::ClockPong,
            5 => Kind::Hello,
            6 => Kind::HelloAck,
            7 => Kind::Bye,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("datagram too short")]
    Truncated,
    #[error("bad magic byte")]
    BadMagic,
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u8),
    #[error("unknown packet kind {0}")]
    UnknownKind(u8),
    #[error("invalid field: {0}")]
    Invalid(&'static str),
}

/// Header of one video shard. The shard bytes follow directly.
///
/// A frame of `frame_len` bytes is split into `group_count` FEC groups.
/// Group `group_index` covers the bytes starting at `group_offset` and is
/// carried by `data_shards` data shards plus `recovery_shards`
/// Reed-Solomon recovery shards, all of the same (even) size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoHeader {
    pub keyframe: bool,
    pub session_id: u32,
    pub frame_id: u32,
    pub frame_len: u32,
    /// Host clock, microseconds: when the frame was captured.
    pub capture_us: u64,
    /// Microseconds after `capture_us` at which the captured frame was ready.
    pub capture_ready_delta_us: u32,
    /// Microseconds after `capture_us` at which encoding finished.
    pub encoded_delta_us: u32,
    pub group_offset: u32,
    pub group_index: u16,
    pub group_count: u16,
    pub shard_index: u16,
    pub data_shards: u16,
    pub recovery_shards: u16,
    pub slice_index: u8,
    pub slice_count: u8,
}

const FLAG_KEYFRAME: u8 = 0x01;
const FLAG_REQUEST_KEYFRAME: u8 = 0x01;

impl VideoHeader {
    pub const LEN: usize = PREFIX_LEN + 44;

    /// Writes the header into `buf[..Self::LEN]`.
    pub fn write(&self, buf: &mut [u8]) {
        let flags = if self.keyframe { FLAG_KEYFRAME } else { 0 };
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        w.prefix(Kind::Video, flags);
        w.u32(self.session_id);
        w.u32(self.frame_id);
        w.u32(self.frame_len);
        w.u64(self.capture_us);
        w.u32(self.capture_ready_delta_us);
        w.u32(self.encoded_delta_us);
        w.u32(self.group_offset);
        w.u16(self.group_index);
        w.u16(self.group_count);
        w.u16(self.shard_index);
        w.u16(self.data_shards);
        w.u16(self.recovery_shards);
        w.u8(self.slice_index);
        w.u8(self.slice_count);
    }

    fn read(flags: u8, r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let h = VideoHeader {
            keyframe: flags & FLAG_KEYFRAME != 0,
            session_id: r.u32()?,
            frame_id: r.u32()?,
            frame_len: r.u32()?,
            capture_us: r.u64()?,
            capture_ready_delta_us: r.u32()?,
            encoded_delta_us: r.u32()?,
            group_offset: r.u32()?,
            group_index: r.u16()?,
            group_count: r.u16()?,
            shard_index: r.u16()?,
            data_shards: r.u16()?,
            recovery_shards: r.u16()?,
            slice_index: r.u8()?,
            slice_count: r.u8()?,
        };
        h.validate()?;
        Ok(h)
    }

    fn validate(&self) -> Result<(), DecodeError> {
        use DecodeError::Invalid;
        if self.frame_len == 0 {
            return Err(Invalid("frame_len is zero"));
        }
        if self.group_count == 0 || self.group_index >= self.group_count {
            return Err(Invalid("group_index out of range"));
        }
        if self.group_offset >= self.frame_len {
            return Err(Invalid("group_offset beyond frame"));
        }
        if self.data_shards == 0 {
            return Err(Invalid("no data shards"));
        }
        if u32::from(self.shard_index)
            >= u32::from(self.data_shards) + u32::from(self.recovery_shards)
        {
            return Err(Invalid("shard_index out of range"));
        }
        if self.slice_count == 0 || self.slice_index >= self.slice_count {
            return Err(Invalid("slice_index out of range"));
        }
        Ok(())
    }

    pub fn is_recovery(&self) -> bool {
        self.shard_index >= self.data_shards
    }
}

/// Periodic receiver report, client → host. Counters cover the interval
/// since the previous report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Feedback {
    pub session_id: u32,
    pub request_keyframe: bool,
    pub highest_frame_id: u32,
    pub frames_completed: u32,
    pub frames_dropped: u32,
    pub packets_received: u32,
    pub packets_lost: u32,
    pub packets_recovered: u32,
}

impl Feedback {
    pub const LEN: usize = PREFIX_LEN + 28;

    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let flags = if self.request_keyframe {
            FLAG_REQUEST_KEYFRAME
        } else {
            0
        };
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        w.prefix(Kind::Feedback, flags);
        w.u32(self.session_id);
        w.u32(self.highest_frame_id);
        w.u32(self.frames_completed);
        w.u32(self.frames_dropped);
        w.u32(self.packets_received);
        w.u32(self.packets_lost);
        w.u32(self.packets_recovered);
        Self::LEN
    }

    fn read(flags: u8, r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Feedback {
            request_keyframe: flags & FLAG_REQUEST_KEYFRAME != 0,
            session_id: r.u32()?,
            highest_frame_id: r.u32()?,
            frames_completed: r.u32()?,
            frames_dropped: r.u32()?,
            packets_received: r.u32()?,
            packets_lost: r.u32()?,
            packets_recovered: r.u32()?,
        })
    }

    /// Fraction of packets lost on the wire in this interval (0.0–1.0).
    pub fn loss_ratio(&self) -> f32 {
        let total = self.packets_received as u64 + self.packets_lost as u64;
        if total == 0 {
            0.0
        } else {
            self.packets_lost as f32 / total as f32
        }
    }
}

/// Clock-sync request, client → host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockPing {
    pub seq: u32,
    pub client_send_us: u64,
}

impl ClockPing {
    pub const LEN: usize = PREFIX_LEN + 12;

    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        w.prefix(Kind::ClockPing, 0);
        w.u32(self.seq);
        w.u64(self.client_send_us);
        Self::LEN
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(ClockPing {
            seq: r.u32()?,
            client_send_us: r.u64()?,
        })
    }
}

/// Clock-sync reply, host → client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockPong {
    pub seq: u32,
    pub client_send_us: u64,
    pub host_recv_us: u64,
    pub host_send_us: u64,
}

impl ClockPong {
    pub const LEN: usize = PREFIX_LEN + 28;

    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        w.prefix(Kind::ClockPong, 0);
        w.u32(self.seq);
        w.u64(self.client_send_us);
        w.u64(self.host_recv_us);
        w.u64(self.host_send_us);
        Self::LEN
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(ClockPong {
            seq: r.u32()?,
            client_send_us: r.u64()?,
            host_recv_us: r.u64()?,
            host_send_us: r.u64()?,
        })
    }
}

/// Session request, client → host. Repeated until a [`HelloAck`] arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hello {
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_kbps: u32,
}

impl Hello {
    pub const LEN: usize = PREFIX_LEN + 10;

    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        w.prefix(Kind::Hello, 0);
        w.u16(self.width);
        w.u16(self.height);
        w.u16(self.fps);
        w.u32(self.bitrate_kbps);
        Self::LEN
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let h = Hello {
            width: r.u16()?,
            height: r.u16()?,
            fps: r.u16()?,
            bitrate_kbps: r.u32()?,
        };
        if h.fps == 0 {
            return Err(DecodeError::Invalid("fps is zero"));
        }
        Ok(h)
    }
}

/// Session acceptance, host → client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HelloAck {
    pub session_id: u32,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub codec: Codec,
}

/// Video codec carried in the session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    /// Synthetic test payload (no real video), used until the VAAPI backend lands.
    #[default]
    Synthetic = 0,
    H264 = 1,
    Hevc = 2,
    Av1 = 3,
}

impl Codec {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Codec::Synthetic,
            1 => Codec::H264,
            2 => Codec::Hevc,
            3 => Codec::Av1,
            _ => return None,
        })
    }
}

impl HelloAck {
    pub const LEN: usize = PREFIX_LEN + 11;

    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        w.prefix(Kind::HelloAck, 0);
        w.u32(self.session_id);
        w.u16(self.width);
        w.u16(self.height);
        w.u16(self.fps);
        w.u8(self.codec as u8);
        Self::LEN
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(HelloAck {
            session_id: r.u32()?,
            width: r.u16()?,
            height: r.u16()?,
            fps: r.u16()?,
            codec: Codec::from_u8(r.u8()?).ok_or(DecodeError::Invalid("unknown codec"))?,
        })
    }
}

/// Session teardown, either direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bye {
    pub session_id: u32,
}

impl Bye {
    pub const LEN: usize = PREFIX_LEN + 4;

    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        w.prefix(Kind::Bye, 0);
        w.u32(self.session_id);
        Self::LEN
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Bye {
            session_id: r.u32()?,
        })
    }
}

/// A parsed datagram. Video payloads borrow from the input buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Video(VideoHeader, &'a [u8]),
    Feedback(Feedback),
    ClockPing(ClockPing),
    ClockPong(ClockPong),
    Hello(Hello),
    HelloAck(HelloAck),
    Bye(Bye),
}

impl<'a> Packet<'a> {
    pub fn decode(buf: &'a [u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(buf);
        if r.u8()? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = r.u8()?;
        if version != VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let kind_byte = r.u8()?;
        let kind = Kind::from_u8(kind_byte).ok_or(DecodeError::UnknownKind(kind_byte))?;
        let flags = r.u8()?;
        Ok(match kind {
            Kind::Video => {
                let h = VideoHeader::read(flags, &mut r)?;
                let payload = r.rest();
                if payload.is_empty() || payload.len() % 2 != 0 {
                    return Err(DecodeError::Invalid("shard size must be even and non-zero"));
                }
                Packet::Video(h, payload)
            }
            Kind::Feedback => Packet::Feedback(Feedback::read(flags, &mut r)?),
            Kind::ClockPing => Packet::ClockPing(ClockPing::read(&mut r)?),
            Kind::ClockPong => Packet::ClockPong(ClockPong::read(&mut r)?),
            Kind::Hello => Packet::Hello(Hello::read(&mut r)?),
            Kind::HelloAck => Packet::HelloAck(HelloAck::read(&mut r)?),
            Kind::Bye => Packet::Bye(Bye::read(&mut r)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_header() -> VideoHeader {
        VideoHeader {
            keyframe: true,
            session_id: 0xDEAD_BEEF,
            frame_id: 42,
            frame_len: 5000,
            capture_us: 123_456_789,
            capture_ready_delta_us: 900,
            encoded_delta_us: 4_100,
            group_offset: 0,
            group_index: 0,
            group_count: 1,
            shard_index: 4,
            data_shards: 4,
            recovery_shards: 2,
            slice_index: 0,
            slice_count: 1,
        }
    }

    #[test]
    fn video_roundtrip() {
        let h = video_header();
        let mut buf = vec![0u8; VideoHeader::LEN + 8];
        h.write(&mut buf);
        buf[VideoHeader::LEN..].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        match Packet::decode(&buf).unwrap() {
            Packet::Video(got, payload) => {
                assert_eq!(got, h);
                assert!(got.is_recovery());
                assert_eq!(payload, &[1, 2, 3, 4, 5, 6, 7, 8]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn video_rejects_bad_fields() {
        let mut buf = vec![0u8; VideoHeader::LEN + 2];
        let mut h = video_header();
        h.shard_index = 6;
        h.write(&mut buf);
        assert!(matches!(Packet::decode(&buf), Err(DecodeError::Invalid(_))));

        let mut h = video_header();
        h.group_index = 1;
        h.write(&mut buf);
        assert!(matches!(Packet::decode(&buf), Err(DecodeError::Invalid(_))));

        // odd payload
        let mut odd = vec![0u8; VideoHeader::LEN + 3];
        video_header().write(&mut odd);
        assert!(matches!(Packet::decode(&odd), Err(DecodeError::Invalid(_))));
    }

    #[test]
    fn control_roundtrips() {
        let mut buf = [0u8; MAX_DATAGRAM];

        let fb = Feedback {
            session_id: 7,
            request_keyframe: true,
            highest_frame_id: 99,
            frames_completed: 60,
            frames_dropped: 1,
            packets_received: 990,
            packets_lost: 10,
            packets_recovered: 9,
        };
        let n = fb.encode(&mut buf);
        assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::Feedback(fb));
        assert!((fb.loss_ratio() - 0.01).abs() < 1e-6);

        let ping = ClockPing {
            seq: 3,
            client_send_us: 1_000,
        };
        let n = ping.encode(&mut buf);
        assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::ClockPing(ping));

        let pong = ClockPong {
            seq: 3,
            client_send_us: 1_000,
            host_recv_us: 50_000,
            host_send_us: 50_010,
        };
        let n = pong.encode(&mut buf);
        assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::ClockPong(pong));

        let hello = Hello {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
        };
        let n = hello.encode(&mut buf);
        assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::Hello(hello));

        let ack = HelloAck {
            session_id: 9,
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
        };
        let n = ack.encode(&mut buf);
        assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::HelloAck(ack));

        let bye = Bye { session_id: 9 };
        let n = bye.encode(&mut buf);
        assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::Bye(bye));
    }

    #[test]
    fn rejects_garbage_prefix() {
        assert_eq!(Packet::decode(&[]), Err(DecodeError::Truncated));
        assert_eq!(Packet::decode(&[0, 1, 1, 0]), Err(DecodeError::BadMagic));
        assert_eq!(
            Packet::decode(&[MAGIC, 9, 1, 0]),
            Err(DecodeError::UnsupportedVersion(9))
        );
        assert_eq!(
            Packet::decode(&[MAGIC, VERSION, 200, 0]),
            Err(DecodeError::UnknownKind(200))
        );
        assert_eq!(
            Packet::decode(&[MAGIC, VERSION, Kind::Feedback as u8, 0, 1]),
            Err(DecodeError::Truncated)
        );
    }

    /// Cheap stand-in for `cargo fuzz`: random and mutated datagrams must
    /// never panic.
    #[test]
    fn decode_never_panics() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut seed = vec![0u8; VideoHeader::LEN + 16];
        video_header().write(&mut seed);
        for _ in 0..50_000 {
            let len = (next() % 96) as usize;
            let mut buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = Packet::decode(&buf);
            // Mutate a valid packet so the deeper validation paths get hit.
            buf.clone_from(&seed);
            let i = (next() as usize) % buf.len();
            buf[i] = next() as u8;
            buf.truncate((next() as usize) % (buf.len() + 1));
            let _ = Packet::decode(&buf);
        }
    }
}
