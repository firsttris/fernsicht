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

/// Largest encoded frame accepted (8 MiB). A 4K keyframe at 80 Mbit/s is
/// about 1 MB; the bound keeps a spoofed header from making the receiver
/// allocate gigabytes.
pub const MAX_FRAME_LEN: u32 = 8 * 1024 * 1024;

/// Largest FEC group (data + recovery shards).
pub const MAX_GROUP_SHARDS: u32 = 1024;

/// Default shard size: largest even payload whose video packet still fits
/// a datagram once sealed ([`MAX_PLAIN`]).
pub const DEFAULT_SHARD_SIZE: usize = (MAX_PLAIN - VideoHeader::LEN) & !1;

const _: () = assert!(DEFAULT_SHARD_SIZE.is_multiple_of(2));
const _: () = assert!(VideoHeader::LEN + DEFAULT_SHARD_SIZE <= MAX_PLAIN);

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
    Cursor = 8,
    CursorShape = 9,
    Input = 10,
    InputAck = 11,
    Audio = 12,
    Handshake = 13,
    Sealed = 14,
    Pair = 15,
    Reject = 16,
    Discover = 17,
    Announce = 18,
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
            8 => Kind::Cursor,
            9 => Kind::CursorShape,
            10 => Kind::Input,
            11 => Kind::InputAck,
            12 => Kind::Audio,
            13 => Kind::Handshake,
            14 => Kind::Sealed,
            15 => Kind::Pair,
            16 => Kind::Reject,
            17 => Kind::Discover,
            18 => Kind::Announce,
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
/// Frame ids start at 0 in every session and increase by one per frame
/// (wrapping), so gaps tell the receiver how many frames it missed.
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
const FLAG_REQUEST_CURSOR: u8 = 0x02;

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
        if self.frame_len > MAX_FRAME_LEN {
            return Err(Invalid("frame too large"));
        }
        if u32::from(self.data_shards) + u32::from(self.recovery_shards) > MAX_GROUP_SHARDS {
            return Err(Invalid("FEC group too large"));
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
    /// The client lacks the current pointer image: send it now.
    pub request_cursor: bool,
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
        let mut flags = 0;
        if self.request_keyframe {
            flags |= FLAG_REQUEST_KEYFRAME;
        }
        if self.request_cursor {
            flags |= FLAG_REQUEST_CURSOR;
        }
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
            request_cursor: flags & FLAG_REQUEST_CURSOR != 0,
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

impl Hello {
    /// The first handshake message's payload: this Hello and the client's
    /// wall clock (ms), so the host can tell a replayed recording (older
    /// than the last accepted) from a new connection.
    pub fn encode_with_time(&self, unix_ms: u64) -> [u8; Hello::LEN + 8] {
        let mut b = [0u8; Hello::LEN + 8];
        self.encode(&mut b);
        b[Hello::LEN..].copy_from_slice(&unix_ms.to_le_bytes());
        b
    }

    pub fn decode_with_time(b: &[u8]) -> Option<(Hello, u64)> {
        if b.len() != Hello::LEN + 8 {
            return None;
        }
        let Ok(Packet::Hello(hello)) = Packet::decode(&b[..Hello::LEN]) else {
            return None;
        };
        let ms = u64::from_le_bytes(b[Hello::LEN..].try_into().ok()?);
        Some((hello, ms))
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

/// Largest cursor image side, in pixels (KMS cursor planes are at most
/// 256×256).
pub const MAX_CURSOR_SIZE: u16 = 256;

/// Bytes of cursor image per [`CursorShape`] packet (the last one may be
/// shorter). Fixed, so the receiver knows which pieces it has.
pub const CURSOR_CHUNK: usize = 1024;

const FLAG_CURSOR_VISIBLE: u8 = 0x01;

/// Where the pointer is, host → client, with every captured frame.
///
/// The pointer is not part of the video: it lives on its own hardware
/// plane, and the client draws it on top (later also moved locally for
/// instant feedback).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub session_id: u32,
    pub visible: bool,
    /// Which shape is shown ([`CursorShape::serial`]); 0 = none yet.
    pub shape_serial: u32,
    /// Top-left corner of the cursor image on the captured screen, in
    /// screen pixels; may be negative at the screen edges.
    pub x: i32,
    pub y: i32,
    /// Size of the captured screen, to place the cursor on a scaled video.
    pub screen_width: u16,
    pub screen_height: u16,
}

impl Cursor {
    pub const LEN: usize = PREFIX_LEN + 20;

    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        let flags = if self.visible { FLAG_CURSOR_VISIBLE } else { 0 };
        w.prefix(Kind::Cursor, flags);
        w.u32(self.session_id);
        w.u32(self.shape_serial);
        w.u32(self.x as u32);
        w.u32(self.y as u32);
        w.u16(self.screen_width);
        w.u16(self.screen_height);
        Self::LEN
    }

    fn read(flags: u8, r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Cursor {
            visible: flags & FLAG_CURSOR_VISIBLE != 0,
            session_id: r.u32()?,
            shape_serial: r.u32()?,
            x: r.u32()? as i32,
            y: r.u32()? as i32,
            screen_width: r.u16()?,
            screen_height: r.u16()?,
        })
    }
}

/// A piece of a cursor image, host → client. The image is `width`×`height`
/// pixels, 4 bytes each in DRM `ARGB8888` order (B, G, R, A in memory),
/// premultiplied alpha, rows without padding; this packet carries the bytes
/// from `offset` on (a multiple of [`CURSOR_CHUNK`]). Sent when the shape
/// changes and repeated now and then, so a lost piece heals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorShape {
    pub session_id: u32,
    /// Increases with every new shape; never 0.
    pub serial: u32,
    pub width: u16,
    pub height: u16,
    pub offset: u32,
}

impl CursorShape {
    pub const HEADER_LEN: usize = PREFIX_LEN + 16;

    /// Size of the whole image in bytes.
    pub fn image_len(&self) -> usize {
        usize::from(self.width) * usize::from(self.height) * 4
    }

    /// Writes the header and `data` (at most [`CURSOR_CHUNK`] bytes).
    pub fn encode(&self, data: &[u8], buf: &mut [u8]) -> usize {
        assert!(data.len() <= CURSOR_CHUNK, "cursor chunk too large");
        let len = Self::HEADER_LEN + data.len();
        let mut w = Writer::new(&mut buf[..len]);
        w.prefix(Kind::CursorShape, 0);
        w.u32(self.session_id);
        w.u32(self.serial);
        w.u16(self.width);
        w.u16(self.height);
        w.u32(self.offset);
        w.bytes(data);
        len
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let s = CursorShape {
            session_id: r.u32()?,
            serial: r.u32()?,
            width: r.u16()?,
            height: r.u16()?,
            offset: r.u32()?,
        };
        if s.serial == 0 {
            return Err(DecodeError::Invalid("cursor shape serial 0"));
        }
        if s.width == 0 || s.height == 0 || s.width > MAX_CURSOR_SIZE || s.height > MAX_CURSOR_SIZE
        {
            return Err(DecodeError::Invalid("cursor size"));
        }
        if !(s.offset as usize).is_multiple_of(CURSOR_CHUNK) || s.offset as usize >= s.image_len() {
            return Err(DecodeError::Invalid("cursor chunk offset"));
        }
        Ok(s)
    }
}

/// Most input events in one [`Packet::Input`].
pub const MAX_INPUT_EVENTS: usize = 64;

/// Highest Linux key code (`KEY_MAX`).
pub const KEY_MAX: u16 = 0x2ff;
/// Mouse buttons are Linux codes `BTN_LEFT` (0x110) to `BTN_TASK` (0x117).
pub const BTN_MOUSE_FIRST: u16 = 0x110;
pub const BTN_MOUSE_LAST: u16 = 0x117;

/// One input event from the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// Pointer at a position on the streamed screen, 0..=65535 across its
    /// width and height (desktop use).
    MouseAbs { x: u16, y: u16 },
    /// Pointer moved by this many pixels (games with the pointer captured).
    MouseRel { dx: i32, dy: i32 },
    /// Linux button code, `BTN_LEFT` ..= `BTN_TASK`.
    Button { code: u16, pressed: bool },
    /// Wheel, 120 units per notch (Linux hi-res wheel); positive = up/right.
    Scroll { dx: i32, dy: i32 },
    /// Linux key code (`KEY_*`): the physical key, layout-independent.
    Key { code: u16, pressed: bool },
}

const INPUT_EVENT_LEN: usize = 16;
const _: () = assert!(InputHeader::LEN + MAX_INPUT_EVENTS * INPUT_EVENT_LEN <= MAX_DATAGRAM);
const FLAG_PRESSED: u8 = 0x01;

impl InputEvent {
    fn write(&self, seq: u32, w: &mut Writer<'_>) {
        let (kind, flags, code, a, b) = match *self {
            InputEvent::MouseAbs { x, y } => (1, 0, 0, i32::from(x), i32::from(y)),
            InputEvent::MouseRel { dx, dy } => (2, 0, 0, dx, dy),
            InputEvent::Button { code, pressed } => (3, u8::from(pressed), code, 0, 0),
            InputEvent::Scroll { dx, dy } => (4, 0, 0, dx, dy),
            InputEvent::Key { code, pressed } => (5, u8::from(pressed), code, 0, 0),
        };
        w.u32(seq);
        w.u8(kind);
        w.u8(flags);
        w.u16(code);
        w.u32(a as u32);
        w.u32(b as u32);
    }

    fn read(r: &mut Reader<'_>) -> Result<(u32, Self), DecodeError> {
        let seq = r.u32()?;
        let kind = r.u8()?;
        let flags = r.u8()?;
        let code = r.u16()?;
        let a = r.u32()? as i32;
        let b = r.u32()? as i32;
        let pressed = flags & FLAG_PRESSED != 0;
        // Fields a kind does not use must be zero: one meaning per byte
        // sequence (what parses re-encodes to the same bytes).
        let unused_ok = match kind {
            1 | 2 | 4 => flags == 0 && code == 0,
            3 | 5 => flags & !FLAG_PRESSED == 0 && a == 0 && b == 0,
            _ => true,
        };
        if !unused_ok {
            return Err(DecodeError::Invalid("unused input event field set"));
        }
        let event = match kind {
            1 => InputEvent::MouseAbs {
                x: u16::try_from(a).map_err(|_| DecodeError::Invalid("pointer x"))?,
                y: u16::try_from(b).map_err(|_| DecodeError::Invalid("pointer y"))?,
            },
            2 => InputEvent::MouseRel { dx: a, dy: b },
            3 if (BTN_MOUSE_FIRST..=BTN_MOUSE_LAST).contains(&code) => {
                InputEvent::Button { code, pressed }
            }
            3 => return Err(DecodeError::Invalid("mouse button code")),
            4 => InputEvent::Scroll { dx: a, dy: b },
            5 if (1..=KEY_MAX).contains(&code)
                && !(BTN_MOUSE_FIRST..=BTN_MOUSE_LAST).contains(&code) =>
            {
                InputEvent::Key { code, pressed }
            }
            5 => return Err(DecodeError::Invalid("key code")),
            _ => return Err(DecodeError::Invalid("unknown input event")),
        };
        Ok((seq, event))
    }
}

/// Input events, client → host. Every packet carries all events the host
/// has not acknowledged yet ([`InputAck`]), oldest first, each with its
/// sequence number, so a lost packet costs no key press; the host applies
/// each sequence number once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputHeader {
    pub session_id: u32,
    pub count: u8,
}

impl InputHeader {
    pub const LEN: usize = PREFIX_LEN + 5;

    /// Encodes `events` (at most [`MAX_INPUT_EVENTS`]) into `buf`.
    pub fn encode(session_id: u32, events: &[(u32, InputEvent)], buf: &mut [u8]) -> usize {
        assert!(events.len() <= MAX_INPUT_EVENTS, "too many input events");
        let len = Self::LEN + events.len() * INPUT_EVENT_LEN;
        let mut w = Writer::new(&mut buf[..len]);
        w.prefix(Kind::Input, 0);
        w.u32(session_id);
        w.u8(events.len() as u8);
        for (seq, e) in events {
            e.write(*seq, &mut w);
        }
        len
    }

    /// The events of a packet that [`Packet::decode`] accepted.
    pub fn events(body: &[u8]) -> impl Iterator<Item = (u32, InputEvent)> + '_ {
        body.as_chunks::<INPUT_EVENT_LEN>()
            .0
            .iter()
            .filter_map(|c| InputEvent::read(&mut Reader::new(c)).ok())
    }

    fn read<'a>(r: &mut Reader<'a>) -> Result<(Self, &'a [u8]), DecodeError> {
        let h = InputHeader {
            session_id: r.u32()?,
            count: r.u8()?,
        };
        if h.count == 0 || usize::from(h.count) > MAX_INPUT_EVENTS {
            return Err(DecodeError::Invalid("input event count"));
        }
        let body = r.take_slice(usize::from(h.count) * INPUT_EVENT_LEN)?;
        // Check every event now, so iterating later cannot fail.
        for c in body.as_chunks::<INPUT_EVENT_LEN>().0 {
            InputEvent::read(&mut Reader::new(c))?;
        }
        if !r.is_empty() {
            return Err(DecodeError::Invalid("trailing bytes after input events"));
        }
        Ok((h, body))
    }
}

/// The highest input sequence number the host has applied, host → client.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputAck {
    pub session_id: u32,
    pub seq: u32,
}

impl InputAck {
    pub const LEN: usize = PREFIX_LEN + 8;

    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        w.prefix(Kind::InputAck, 0);
        w.u32(self.session_id);
        w.u32(self.seq);
        Self::LEN
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(InputAck {
            session_id: r.u32()?,
            seq: r.u32()?,
        })
    }
}

/// Largest encoded audio frame (Opus at up to ~500 kbit/s, 5 ms).
pub const MAX_AUDIO_FRAME: usize = 400;

/// Audio, host → client: Opus frame `seq` and, again, frame `seq - 1`, so
/// one lost packet leaves no gap. Frames are 5 ms; `capture_us` (host
/// clock) is when frame `seq` was played on the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioHeader {
    pub session_id: u32,
    pub seq: u32,
    pub capture_us: u64,
}

impl AudioHeader {
    pub const LEN: usize = PREFIX_LEN + 16;

    /// Header, then `frame` and `previous` (may be empty), each with a
    /// 16-bit length.
    pub fn encode(&self, frame: &[u8], previous: &[u8], buf: &mut [u8]) -> usize {
        assert!(
            !frame.is_empty()
                && frame.len() <= MAX_AUDIO_FRAME
                && previous.len() <= MAX_AUDIO_FRAME,
            "audio frame size"
        );
        let len = Self::LEN + 4 + frame.len() + previous.len();
        let mut w = Writer::new(&mut buf[..len]);
        w.prefix(Kind::Audio, 0);
        w.u32(self.session_id);
        w.u32(self.seq);
        w.u64(self.capture_us);
        w.u16(frame.len() as u16);
        w.bytes(frame);
        w.u16(previous.len() as u16);
        w.bytes(previous);
        len
    }

    #[allow(clippy::type_complexity)]
    fn read<'a>(r: &mut Reader<'a>) -> Result<(Self, &'a [u8], &'a [u8]), DecodeError> {
        let h = AudioHeader {
            session_id: r.u32()?,
            seq: r.u32()?,
            capture_us: r.u64()?,
        };
        let mut frame = || -> Result<&'a [u8], DecodeError> {
            let n = usize::from(r.u16()?);
            if n > MAX_AUDIO_FRAME {
                return Err(DecodeError::Invalid("audio frame too large"));
            }
            r.take_slice(n)
        };
        let current = frame()?;
        let previous = frame()?;
        if current.is_empty() {
            return Err(DecodeError::Invalid("empty audio frame"));
        }
        if !r.is_empty() {
            return Err(DecodeError::Invalid("trailing bytes after audio"));
        }
        Ok((h, current, previous))
    }
}

/// Largest Noise handshake message carried.
pub const MAX_HANDSHAKE: usize = 1024;
/// Largest pairing message carried.
pub const MAX_PAIR: usize = 256;

/// One step of a Noise handshake (see the `fernsicht-secure` crate):
/// `reply` false for the client's first message, true for the host's
/// answer. The bytes are opaque here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handshake<'a> {
    pub reply: bool,
    pub message: &'a [u8],
}

const FLAG_REPLY: u8 = 0x01;

impl Handshake<'_> {
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        assert!(
            !self.message.is_empty() && self.message.len() <= MAX_HANDSHAKE,
            "handshake size"
        );
        let len = PREFIX_LEN + self.message.len();
        let mut w = Writer::new(&mut buf[..len]);
        w.prefix(Kind::Handshake, if self.reply { FLAG_REPLY } else { 0 });
        w.bytes(self.message);
        len
    }
}

/// An encrypted packet of a session: `counter` is its nonce; the opened
/// contents are a whole packet of another kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealedHeader {
    pub session_id: u32,
    pub counter: u64,
}

impl SealedHeader {
    pub const LEN: usize = PREFIX_LEN + 12;

    /// Writes the header; the sealed bytes go right after it.
    pub fn write(&self, buf: &mut [u8]) {
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        w.prefix(Kind::Sealed, 0);
        w.u32(self.session_id);
        w.u64(self.counter);
    }
}

/// Bytes sealing adds to a packet: header and the 16-byte tag.
pub const SEALED_OVERHEAD: usize = SealedHeader::LEN + 16;
/// Largest packet that still fits a datagram once sealed.
pub const MAX_PLAIN: usize = MAX_DATAGRAM - SEALED_OVERHEAD;

/// One step (1–4) of pairing with a PIN; the bytes are opaque here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pair<'a> {
    pub step: u8,
    pub message: &'a [u8],
}

impl Pair<'_> {
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        assert!(
            (1..=4).contains(&self.step)
                && !self.message.is_empty()
                && self.message.len() <= MAX_PAIR,
            "pairing message"
        );
        let len = PREFIX_LEN + self.message.len();
        let mut w = Writer::new(&mut buf[..len]);
        w.prefix(Kind::Pair, self.step);
        w.bytes(self.message);
        len
    }
}

/// Why the host turned a request down, host → client, unauthenticated: a
/// hint for the user, never trusted for anything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RejectReason {
    /// This client is not paired with the host.
    NotPaired = 1,
    /// The host is not in pairing mode.
    PairingClosed = 2,
    /// Too many wrong PINs: pairing mode was closed.
    TooManyAttempts = 3,
}

impl RejectReason {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => RejectReason::NotPaired,
            2 => RejectReason::PairingClosed,
            3 => RejectReason::TooManyAttempts,
            _ => return None,
        })
    }

    pub fn encode(self, buf: &mut [u8]) -> usize {
        let len = PREFIX_LEN + 1;
        let mut w = Writer::new(&mut buf[..len]);
        w.prefix(Kind::Reject, 0);
        w.u8(self as u8);
        len
    }
}

/// "Which hosts are there?", client → broadcast, unauthenticated. Padded
/// to [`Discover::LEN`], longer than any [`Announce`], so answering never
/// sends more than was asked (no use for reflection attacks).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Discover {
    /// Echoed in the answer, to tell answers to this question apart.
    pub nonce: u64,
}

impl Discover {
    pub const LEN: usize = 256;

    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let mut w = Writer::new(&mut buf[..Self::LEN]);
        w.prefix(Kind::Discover, 0);
        w.u64(self.nonce);
        w.bytes(&[0; Self::LEN - PREFIX_LEN - 8]);
        Self::LEN
    }
}

/// Longest host name in an [`Announce`].
pub const MAX_ANNOUNCE_NAME: usize = 64;
/// Longest OS and GPU description in an [`Announce`].
pub const MAX_ANNOUNCE_INFO: usize = 48;

/// A host's answer to [`Discover`], unauthenticated: what it says it is.
/// Its key is proven only by the handshake when connecting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Announce<'a> {
    pub nonce: u64,
    /// The host's public key (match it against paired hosts).
    pub key: [u8; 32],
    /// Pairing is open: a PIN is shown on the host.
    pub pairing: bool,
    /// A client is connected.
    pub busy: bool,
    pub name: &'a str,
    /// E.g. "Bazzite".
    pub os: &'a str,
    /// GPU and encoder, e.g. "Radeon RX 7800 XT · H.264".
    pub gpu: &'a str,
}

const FLAG_PAIRING: u8 = 0x01;
const FLAG_BUSY: u8 = 0x02;

/// The longest start of `s` that fits `max` bytes, cut at a character.
pub fn clip(s: &str, max: usize) -> &str {
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

impl Announce<'_> {
    pub const MAX_LEN: usize = PREFIX_LEN + 8 + 32 + 3 + MAX_ANNOUNCE_NAME + 2 * MAX_ANNOUNCE_INFO;

    /// Panics if a text is too long (see [`clip`]) or the name is empty.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        assert!(
            !self.name.is_empty()
                && self.name.len() <= MAX_ANNOUNCE_NAME
                && self.os.len() <= MAX_ANNOUNCE_INFO
                && self.gpu.len() <= MAX_ANNOUNCE_INFO,
            "announce texts"
        );
        let len = PREFIX_LEN + 8 + 32 + 3 + self.name.len() + self.os.len() + self.gpu.len();
        let mut w = Writer::new(&mut buf[..len]);
        let flags =
            if self.pairing { FLAG_PAIRING } else { 0 } | if self.busy { FLAG_BUSY } else { 0 };
        w.prefix(Kind::Announce, flags);
        w.u64(self.nonce);
        w.bytes(&self.key);
        for s in [self.name, self.os, self.gpu] {
            w.u8(s.len() as u8);
            w.bytes(s.as_bytes());
        }
        len
    }
}

const _: () = assert!(Announce::MAX_LEN < Discover::LEN);

fn read_text<'a>(r: &mut Reader<'a>, max: usize) -> Result<&'a str, DecodeError> {
    let n = usize::from(r.u8()?);
    if n > max {
        return Err(DecodeError::Invalid("announce text too long"));
    }
    std::str::from_utf8(r.take_slice(n)?).map_err(|_| DecodeError::Invalid("announce text"))
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
    Cursor(Cursor),
    /// Header and this piece's bytes.
    CursorShape(CursorShape, &'a [u8]),
    /// Header and the checked events; iterate with [`InputHeader::events`].
    Input(InputHeader, &'a [u8]),
    InputAck(InputAck),
    /// Header, frame `seq`, frame `seq - 1` (may be empty).
    Audio(AudioHeader, &'a [u8], &'a [u8]),
    Handshake(Handshake<'a>),
    /// Header and the sealed bytes (open them with the session's keys).
    Sealed(SealedHeader, &'a [u8]),
    Pair(Pair<'a>),
    Reject(RejectReason),
    Discover(Discover),
    Announce(Announce<'a>),
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
                if payload.is_empty() || !payload.len().is_multiple_of(2) {
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
            Kind::Cursor => Packet::Cursor(Cursor::read(flags, &mut r)?),
            Kind::CursorShape => {
                let s = CursorShape::read(&mut r)?;
                let data = r.rest();
                let expected = (s.image_len() - s.offset as usize).min(CURSOR_CHUNK);
                if data.len() != expected {
                    return Err(DecodeError::Invalid("cursor chunk length"));
                }
                Packet::CursorShape(s, data)
            }
            Kind::Input => {
                let (h, body) = InputHeader::read(&mut r)?;
                Packet::Input(h, body)
            }
            Kind::InputAck => Packet::InputAck(InputAck::read(&mut r)?),
            Kind::Audio => {
                let (h, frame, previous) = AudioHeader::read(&mut r)?;
                Packet::Audio(h, frame, previous)
            }
            Kind::Handshake => {
                if flags & !FLAG_REPLY != 0 {
                    return Err(DecodeError::Invalid("handshake flags"));
                }
                let message = r.rest();
                if message.is_empty() || message.len() > MAX_HANDSHAKE {
                    return Err(DecodeError::Invalid("handshake size"));
                }
                Packet::Handshake(Handshake {
                    reply: flags & FLAG_REPLY != 0,
                    message,
                })
            }
            Kind::Sealed => {
                let h = SealedHeader {
                    session_id: r.u32()?,
                    counter: r.u64()?,
                };
                let sealed = r.rest();
                if sealed.len() < 16 {
                    return Err(DecodeError::Invalid("sealed packet too short"));
                }
                Packet::Sealed(h, sealed)
            }
            Kind::Pair => {
                let message = r.rest();
                if !(1..=4).contains(&flags) || message.is_empty() || message.len() > MAX_PAIR {
                    return Err(DecodeError::Invalid("pairing message"));
                }
                Packet::Pair(Pair {
                    step: flags,
                    message,
                })
            }
            Kind::Reject => {
                let reason = RejectReason::from_u8(r.u8()?)
                    .ok_or(DecodeError::Invalid("unknown reject reason"))?;
                if !r.is_empty() {
                    return Err(DecodeError::Invalid("trailing bytes after reject"));
                }
                Packet::Reject(reason)
            }
            Kind::Discover => {
                let nonce = r.u64()?;
                let padding = r.rest();
                if flags != 0
                    || padding.len() != Discover::LEN - PREFIX_LEN - 8
                    || padding.iter().any(|&b| b != 0)
                {
                    return Err(DecodeError::Invalid("discover padding"));
                }
                Packet::Discover(Discover { nonce })
            }
            Kind::Announce => {
                if flags & !(FLAG_PAIRING | FLAG_BUSY) != 0 {
                    return Err(DecodeError::Invalid("announce flags"));
                }
                let nonce = r.u64()?;
                let mut key = [0u8; 32];
                key.copy_from_slice(r.take_slice(32)?);
                let name = read_text(&mut r, MAX_ANNOUNCE_NAME)?;
                let os = read_text(&mut r, MAX_ANNOUNCE_INFO)?;
                let gpu = read_text(&mut r, MAX_ANNOUNCE_INFO)?;
                if name.is_empty() || !r.is_empty() {
                    return Err(DecodeError::Invalid("announce"));
                }
                Packet::Announce(Announce {
                    nonce,
                    key,
                    pairing: flags & FLAG_PAIRING != 0,
                    busy: flags & FLAG_BUSY != 0,
                    name,
                    os,
                    gpu,
                })
            }
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
    fn rejects_oversized_frames_and_groups() {
        // Regression (found by cargo-fuzz): a spoofed frame_len of ~4 GB
        // made the receiver allocate the whole frame up front.
        let mut buf = vec![0u8; VideoHeader::LEN + 2];
        let h = VideoHeader {
            frame_len: MAX_FRAME_LEN + 1,
            ..video_header()
        };
        h.write(&mut buf);
        assert_eq!(
            Packet::decode(&buf),
            Err(DecodeError::Invalid("frame too large"))
        );
        let h = VideoHeader {
            data_shards: 1000,
            recovery_shards: 100,
            shard_index: 0,
            ..video_header()
        };
        h.write(&mut buf);
        assert_eq!(
            Packet::decode(&buf),
            Err(DecodeError::Invalid("FEC group too large"))
        );
    }

    #[test]
    fn control_roundtrips() {
        let mut buf = [0u8; MAX_DATAGRAM];

        let fb = Feedback {
            session_id: 7,
            request_keyframe: true,
            request_cursor: true,
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
    fn cursor_roundtrips() {
        let mut buf = [0u8; MAX_DATAGRAM];
        let c = Cursor {
            session_id: 3,
            visible: true,
            shape_serial: 7,
            x: -12,
            y: 1439,
            screen_width: 2560,
            screen_height: 1440,
        };
        let n = c.encode(&mut buf);
        assert_eq!(n, Cursor::LEN);
        assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::Cursor(c));
        let hidden = Cursor {
            visible: false,
            ..c
        };
        let n = hidden.encode(&mut buf);
        assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::Cursor(hidden));
    }

    #[test]
    fn cursor_shape_chunks_roundtrip_and_are_checked() {
        let mut buf = [0u8; MAX_DATAGRAM];
        // 24×24 cursor: 2304 bytes in pieces of 1024, 1024, 256.
        let image: Vec<u8> = (0..24 * 24 * 4).map(|i| i as u8).collect();
        let mut got = vec![0u8; image.len()];
        for (i, chunk) in image.chunks(CURSOR_CHUNK).enumerate() {
            let s = CursorShape {
                session_id: 1,
                serial: 5,
                width: 24,
                height: 24,
                offset: (i * CURSOR_CHUNK) as u32,
            };
            let n = s.encode(chunk, &mut buf);
            assert!(n <= MAX_DATAGRAM);
            let Packet::CursorShape(h, data) = Packet::decode(&buf[..n]).unwrap() else {
                panic!("not a cursor shape")
            };
            assert_eq!(h, s);
            got[h.offset as usize..][..data.len()].copy_from_slice(data);
        }
        assert_eq!(got, image);

        let good = CursorShape {
            session_id: 1,
            serial: 5,
            width: 24,
            height: 24,
            offset: 0,
        };
        let bad = [
            (CursorShape { serial: 0, ..good }, 1024),
            (CursorShape { width: 0, ..good }, 1024),
            (CursorShape { width: 257, ..good }, 1024),
            (
                CursorShape {
                    offset: 100,
                    ..good
                },
                1024,
            ),
            (
                CursorShape {
                    offset: 3072,
                    ..good
                },
                256,
            ),
            (good, 1000),
            (
                CursorShape {
                    offset: 2048,
                    ..good
                },
                1024,
            ),
        ];
        for (s, len) in bad {
            let n = s.encode(&vec![0; len.min(CURSOR_CHUNK)], &mut buf);
            assert!(Packet::decode(&buf[..n]).is_err(), "{s:?} with {len} bytes");
        }
    }

    #[test]
    fn input_roundtrips_in_order() {
        let mut buf = [0u8; MAX_DATAGRAM];
        let events = [
            (7, InputEvent::MouseAbs { x: 0, y: 65535 }),
            (8, InputEvent::MouseRel { dx: -5, dy: 12 }),
            (
                9,
                InputEvent::Button {
                    code: 0x110,
                    pressed: true,
                },
            ),
            (10, InputEvent::Scroll { dx: 0, dy: -240 }),
            (
                11,
                InputEvent::Key {
                    code: 30,
                    pressed: true,
                },
            ),
            (
                12,
                InputEvent::Key {
                    code: 30,
                    pressed: false,
                },
            ),
        ];
        let n = InputHeader::encode(3, &events, &mut buf);
        let Packet::Input(h, body) = Packet::decode(&buf[..n]).unwrap() else {
            panic!("not input")
        };
        assert_eq!((h.session_id, h.count), (3, 6));
        assert_eq!(InputHeader::events(body).collect::<Vec<_>>(), events);

        let ack = InputAck {
            session_id: 3,
            seq: 12,
        };
        let n = ack.encode(&mut buf);
        assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::InputAck(ack));

        let full: Vec<_> = (0..MAX_INPUT_EVENTS as u32)
            .map(|i| (i, InputEvent::MouseRel { dx: 1, dy: 1 }))
            .collect();
        let n = InputHeader::encode(1, &full, &mut buf);
        assert!(n <= MAX_DATAGRAM);
        assert!(Packet::decode(&buf[..n]).is_ok());
    }

    #[test]
    fn audio_roundtrips_with_its_previous_frame() {
        let mut buf = [0u8; MAX_DATAGRAM];
        let h = AudioHeader {
            session_id: 4,
            seq: 77,
            capture_us: 123_456_789,
        };
        let (cur, prev) = ([1u8; 80], [2u8; 81]);
        let n = h.encode(&cur, &prev, &mut buf);
        assert_eq!(
            Packet::decode(&buf[..n]).unwrap(),
            Packet::Audio(h, &cur[..], &prev[..])
        );
        // The first frame has no previous one.
        let n = h.encode(&cur, &[], &mut buf);
        assert_eq!(
            Packet::decode(&buf[..n]).unwrap(),
            Packet::Audio(h, &cur[..], &[][..])
        );
        // Two maximal frames fit a datagram.
        let big = [0u8; MAX_AUDIO_FRAME];
        assert!(h.encode(&big, &big, &mut buf) <= MAX_DATAGRAM);
        // Broken: truncated, trailing, oversized length, empty frame.
        let n = h.encode(&cur, &prev, &mut buf);
        assert!(Packet::decode(&buf[..n - 1]).is_err());
        assert!(Packet::decode(&buf[..n + 1]).is_err());
        let mut huge = buf[..n].to_vec();
        huge[AudioHeader::LEN..AudioHeader::LEN + 2].copy_from_slice(&500u16.to_le_bytes());
        assert!(Packet::decode(&huge).is_err());
        let mut empty = buf[..AudioHeader::LEN].to_vec();
        empty.extend_from_slice(&[0, 0, 0, 0]);
        assert!(Packet::decode(&empty).is_err());
    }

    #[test]
    fn hello_with_time_roundtrips() {
        let h = Hello {
            width: 2560,
            height: 1440,
            fps: 60,
            bitrate_kbps: 0,
        };
        let b = h.encode_with_time(1_760_000_000_123);
        assert_eq!(Hello::decode_with_time(&b), Some((h, 1_760_000_000_123)));
        assert_eq!(Hello::decode_with_time(&b[..b.len() - 1]), None);
        let mut bad = b;
        bad[0] = 0;
        assert_eq!(Hello::decode_with_time(&bad), None);
    }

    #[test]
    fn security_packets_roundtrip() {
        let mut buf = [0u8; MAX_DATAGRAM];
        for reply in [false, true] {
            let h = Handshake {
                reply,
                message: &[7u8; 96],
            };
            let n = h.encode(&mut buf);
            assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::Handshake(h));
        }
        let s = SealedHeader {
            session_id: 9,
            counter: u64::MAX - 1,
        };
        s.write(&mut buf);
        buf[SealedHeader::LEN..SealedHeader::LEN + 40].fill(3);
        assert_eq!(
            Packet::decode(&buf[..SealedHeader::LEN + 40]).unwrap(),
            Packet::Sealed(s, &[3u8; 40][..])
        );
        assert!(
            Packet::decode(&buf[..SealedHeader::LEN + 15]).is_err(),
            "shorter than a tag"
        );
        for step in 1..=4 {
            let p = Pair {
                step,
                message: &[1, 2, 3],
            };
            let n = p.encode(&mut buf);
            assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::Pair(p));
        }
        for r in [
            RejectReason::NotPaired,
            RejectReason::PairingClosed,
            RejectReason::TooManyAttempts,
        ] {
            let n = r.encode(&mut buf);
            assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::Reject(r));
        }
        // Broken ones.
        assert!(
            Packet::decode(&[MAGIC, VERSION, Kind::Handshake as u8, 0]).is_err(),
            "empty"
        );
        assert!(
            Packet::decode(&[MAGIC, VERSION, Kind::Handshake as u8, 2, 1]).is_err(),
            "flags"
        );
        assert!(
            Packet::decode(&[MAGIC, VERSION, Kind::Pair as u8, 5, 1]).is_err(),
            "step"
        );
        assert!(
            Packet::decode(&[MAGIC, VERSION, Kind::Pair as u8, 0, 1]).is_err(),
            "step 0"
        );
        assert!(Packet::decode(&[MAGIC, VERSION, Kind::Reject as u8, 0, 9]).is_err());
        assert!(Packet::decode(&[MAGIC, VERSION, Kind::Reject as u8, 0, 1, 0]).is_err());
        // Everything sealed fits a datagram.
        assert_eq!(MAX_PLAIN + SEALED_OVERHEAD, MAX_DATAGRAM);
    }

    #[test]
    fn discovery_roundtrips() {
        let mut buf = [0u8; MAX_DATAGRAM];
        let d = Discover { nonce: 0xfeed };
        let n = d.encode(&mut buf);
        assert_eq!(n, Discover::LEN);
        assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::Discover(d));
        // Short, long or with something in the padding: not a question.
        assert!(Packet::decode(&buf[..n - 1]).is_err());
        assert!(Packet::decode(&buf[..n + 1]).is_err());
        buf[100] = 1;
        assert!(Packet::decode(&buf[..n]).is_err());

        let name = "ä".repeat(32);
        let longest = "x".repeat(MAX_ANNOUNCE_INFO);
        for (pairing, busy, os, gpu) in [
            (false, false, "", ""),
            (true, false, "Bazzite", "Radeon RX 7800 XT · H.264"),
            (true, true, longest.as_str(), longest.as_str()),
        ] {
            let a = Announce {
                nonce: 7,
                key: [9; 32],
                pairing,
                busy,
                name: &name,
                os,
                gpu,
            };
            let n = a.encode(&mut buf);
            assert!(n <= Announce::MAX_LEN);
            assert_eq!(Packet::decode(&buf[..n]).unwrap(), Packet::Announce(a));
            assert!(Packet::decode(&buf[..n - 1]).is_err());
        }
        // An empty name, bad UTF-8, unknown flags.
        let a = Announce {
            nonce: 1,
            key: [0; 32],
            pairing: false,
            busy: false,
            name: "h",
            os: "",
            gpu: "",
        };
        let n = a.encode(&mut buf);
        let name_at = PREFIX_LEN + 8 + 32;
        let mut bad = buf;
        bad[name_at] = 0;
        bad[name_at + 1] = 0;
        assert!(Packet::decode(&bad[..n - 1]).is_err(), "empty name");
        let mut bad = buf;
        bad[name_at + 1] = 0xff;
        assert!(Packet::decode(&bad[..n]).is_err(), "not UTF-8");
        let mut bad = buf;
        bad[3] = 4;
        assert!(Packet::decode(&bad[..n]).is_err(), "flags");
        let mut bad = buf;
        bad[name_at] = 65;
        assert!(Packet::decode(&bad[..n]).is_err(), "longer than allowed");
    }

    #[test]
    fn clip_cuts_at_characters() {
        assert_eq!(clip("zentrale", 64), "zentrale");
        assert_eq!(clip("zentrale", 4), "zent");
        assert_eq!(clip("äöü", 3), "ä");
        assert_eq!(clip("", 3), "");
    }

    #[test]
    fn bad_input_is_rejected() {
        let mut buf = [0u8; MAX_DATAGRAM];
        let bad = [
            InputEvent::Button {
                code: 0x10f,
                pressed: true,
            },
            InputEvent::Button {
                code: 0x118,
                pressed: true,
            },
            InputEvent::Key {
                code: 0,
                pressed: true,
            },
            InputEvent::Key {
                code: KEY_MAX + 1,
                pressed: true,
            },
            // A mouse button smuggled in as a key.
            InputEvent::Key {
                code: 0x110,
                pressed: true,
            },
        ];
        for e in bad {
            let n = InputHeader::encode(1, &[(1, e)], &mut buf);
            assert!(Packet::decode(&buf[..n]).is_err(), "{e:?}");
        }
        let n = InputHeader::encode(
            1,
            &[(
                1,
                InputEvent::Key {
                    code: 30,
                    pressed: true,
                },
            )],
            &mut buf,
        );
        // Truncated, trailing bytes, zero events, too many, unknown kind.
        assert!(Packet::decode(&buf[..n - 1]).is_err());
        assert!(Packet::decode(&buf[..n + 1]).is_err());
        let mut zero = buf[..n].to_vec();
        zero[8] = 0;
        assert!(Packet::decode(&zero[..InputHeader::LEN]).is_err());
        let mut many = buf[..n].to_vec();
        many[8] = (MAX_INPUT_EVENTS + 1) as u8;
        assert!(Packet::decode(&many).is_err());
        let mut unknown = buf[..n].to_vec();
        unknown[InputHeader::LEN + 4] = 99;
        assert!(Packet::decode(&unknown).is_err());
        // Unused fields set (found by the fuzzer: a button with motion bytes).
        let crash = [
            0xf5, 0x01, 0x0a, 0x00, 0x00, 0xff, 0xff, 0xdf, 0x01, 0x03, 0x00, 0x01, 0x03, 0x04,
            0xfd, 0xfc, 0xfc, 0xfc, 0xfc, 0xfc, 0xfc, 0x03, 0x00, 0x03, 0xf5,
        ];
        assert!(Packet::decode(&crash).is_err());
        let mut flagged = buf[..n].to_vec();
        flagged[InputHeader::LEN + 5] = 0x03; // pressed + an unknown flag
        assert!(Packet::decode(&flagged).is_err());
        // Pointer coordinates beyond 16 bit.
        let mut wide = buf[..n].to_vec();
        wide[InputHeader::LEN + 4] = 1;
        wide[InputHeader::LEN + 8..InputHeader::LEN + 12].copy_from_slice(&70_000u32.to_le_bytes());
        assert!(Packet::decode(&wide).is_err());
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
