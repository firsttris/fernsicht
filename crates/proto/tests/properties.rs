//! Property tests for the wire format.

use fernsicht_proto::*;
use proptest::prelude::*;

fn video_header() -> impl Strategy<Value = VideoHeader> {
    (
        (
            any::<bool>(),
            any::<u32>(),
            any::<u32>(),
            1u32..=MAX_FRAME_LEN,
        ),
        (any::<u64>(), any::<u32>(), any::<u32>()),
        (
            1u16..=512,
            1u16..=256,
            0u16..=256,
            any::<u16>(),
            any::<u16>(),
        ),
        (1u8..=8, any::<u8>()),
    )
        .prop_map(
            |(
                (keyframe, session_id, frame_id, frame_len),
                (capture_us, ready, encoded),
                (group_count, data_shards, recovery_shards, gi, si),
                (slice_count, sl),
            )| {
                let total = u32::from(data_shards) + u32::from(recovery_shards);
                VideoHeader {
                    keyframe,
                    session_id,
                    frame_id,
                    frame_len,
                    capture_us,
                    capture_ready_delta_us: ready,
                    encoded_delta_us: encoded,
                    group_offset: frame_len / 2,
                    group_index: gi % group_count,
                    group_count,
                    shard_index: (u32::from(si) % total) as u16,
                    data_shards,
                    recovery_shards,
                    slice_index: sl % slice_count,
                    slice_count,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn video_roundtrip(h in video_header(), half in 1usize..700) {
        let payload: Vec<u8> = (0..half * 2).map(|i| i as u8).collect();
        let mut buf = vec![0u8; VideoHeader::LEN + payload.len()];
        h.write(&mut buf);
        buf[VideoHeader::LEN..].copy_from_slice(&payload);
        prop_assert_eq!(Packet::decode(&buf), Ok(Packet::Video(h, &payload[..])));
    }

    #[test]
    fn feedback_roundtrip(
        session_id: u32, request_keyframe: bool, highest_frame_id: u32,
        frames_completed: u32, frames_dropped: u32,
        packets_received: u32, packets_lost: u32, packets_recovered: u32,
    ) {
        let fb = Feedback { session_id, request_keyframe, highest_frame_id, frames_completed,
            frames_dropped, packets_received, packets_lost, packets_recovered };
        let mut buf = [0u8; Feedback::LEN];
        let n = fb.encode(&mut buf);
        prop_assert_eq!(Packet::decode(&buf[..n]), Ok(Packet::Feedback(fb)));
        let ratio = fb.loss_ratio();
        prop_assert!((0.0..=1.0).contains(&ratio));
    }

    #[test]
    fn clock_roundtrip(seq: u32, t0: u64, t1: u64, t2: u64) {
        let mut buf = [0u8; MAX_DATAGRAM];
        let ping = ClockPing { seq, client_send_us: t0 };
        let n = ping.encode(&mut buf);
        prop_assert_eq!(Packet::decode(&buf[..n]), Ok(Packet::ClockPing(ping)));
        let pong = ClockPong { seq, client_send_us: t0, host_recv_us: t1, host_send_us: t2 };
        let n = pong.encode(&mut buf);
        prop_assert_eq!(Packet::decode(&buf[..n]), Ok(Packet::ClockPong(pong)));
    }

    #[test]
    fn session_roundtrip(width: u16, height: u16, fps in 1u16.., bitrate_kbps: u32,
                         session_id: u32, codec in 0u8..4) {
        let mut buf = [0u8; MAX_DATAGRAM];
        let hello = Hello { width, height, fps, bitrate_kbps };
        let n = hello.encode(&mut buf);
        prop_assert_eq!(Packet::decode(&buf[..n]), Ok(Packet::Hello(hello)));
        let codec = [Codec::Synthetic, Codec::H264, Codec::Hevc, Codec::Av1][codec as usize];
        let ack = HelloAck { session_id, width, height, fps, codec };
        let n = ack.encode(&mut buf);
        prop_assert_eq!(Packet::decode(&buf[..n]), Ok(Packet::HelloAck(ack)));
        let bye = Bye { session_id };
        let n = bye.encode(&mut buf);
        prop_assert_eq!(Packet::decode(&buf[..n]), Ok(Packet::Bye(bye)));
    }

    #[test]
    fn cursor_roundtrip(session_id: u32, visible: bool, shape_serial: u32, x: i32, y: i32,
                        screen_width: u16, screen_height: u16) {
        let mut buf = [0u8; Cursor::LEN];
        let c = Cursor { session_id, visible, shape_serial, x, y, screen_width, screen_height };
        let n = c.encode(&mut buf);
        prop_assert_eq!(Packet::decode(&buf[..n]), Ok(Packet::Cursor(c)));
    }

    /// Every piece of any valid cursor image parses back to the same bytes.
    #[test]
    fn cursor_shape_roundtrip(session_id: u32, serial in 1u32.., width in 1u16..=MAX_CURSOR_SIZE,
                              height in 1u16..=MAX_CURSOR_SIZE, seed: u8) {
        let len = usize::from(width) * usize::from(height) * 4;
        let image: Vec<u8> = (0..len).map(|i| (i as u8) ^ seed).collect();
        let mut buf = [0u8; MAX_DATAGRAM];
        let mut got = vec![0u8; len];
        for (i, chunk) in image.chunks(CURSOR_CHUNK).enumerate() {
            let s = CursorShape { session_id, serial, width, height,
                                  offset: (i * CURSOR_CHUNK) as u32 };
            let n = s.encode(chunk, &mut buf);
            let Ok(Packet::CursorShape(h, data)) = Packet::decode(&buf[..n]) else {
                return Err(TestCaseError::fail("piece did not parse"));
            };
            prop_assert_eq!(h, s);
            got[h.offset as usize..][..data.len()].copy_from_slice(data);
        }
        prop_assert_eq!(got, image);
    }

    /// Arbitrary bytes never panic, and whatever parses re-encodes to a
    /// datagram that parses to the same packet (no lossy interpretation).
    #[test]
    fn arbitrary_bytes_are_safe(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
        let Ok(packet) = Packet::decode(&bytes) else { return Ok(()); };
        let mut buf = vec![0u8; MAX_DATAGRAM + 128];
        let n = match packet {
            Packet::Video(h, payload) => {
                h.write(&mut buf);
                buf[VideoHeader::LEN..VideoHeader::LEN + payload.len()].copy_from_slice(payload);
                VideoHeader::LEN + payload.len()
            }
            Packet::Feedback(p) => p.encode(&mut buf),
            Packet::ClockPing(p) => p.encode(&mut buf),
            Packet::ClockPong(p) => p.encode(&mut buf),
            Packet::Hello(p) => p.encode(&mut buf),
            Packet::HelloAck(p) => p.encode(&mut buf),
            Packet::Bye(p) => p.encode(&mut buf),
            Packet::Cursor(p) => p.encode(&mut buf),
            Packet::CursorShape(p, data) => p.encode(data, &mut buf),
        };
        prop_assert_eq!(Packet::decode(&buf[..n]), Ok(packet));
    }

    /// Valid prefix plus random body: exercises the per-kind validation.
    #[test]
    fn random_bodies_are_safe(kind in 1u8..=9, flags: u8,
                              body in proptest::collection::vec(any::<u8>(), 0..96)) {
        let mut bytes = vec![MAGIC, VERSION, kind, flags];
        bytes.extend_from_slice(&body);
        let _ = Packet::decode(&bytes);
    }

    /// Headers beyond the size limits are rejected, never accepted.
    #[test]
    fn oversized_headers_are_rejected(h in video_header(), extra in 1u32..=u32::MAX - MAX_FRAME_LEN) {
        let mut buf = vec![0u8; VideoHeader::LEN + 2];
        VideoHeader { frame_len: MAX_FRAME_LEN + extra, ..h }.write(&mut buf);
        prop_assert!(Packet::decode(&buf).is_err());
    }

    #[test]
    fn truncation_is_an_error(h in video_header(), cut in 0usize..VideoHeader::LEN) {
        let mut buf = vec![0u8; VideoHeader::LEN + 2];
        h.write(&mut buf);
        prop_assert!(Packet::decode(&buf[..cut]).is_err());
    }
}
