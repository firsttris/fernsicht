//! Any datagram: parsing never panics, and whatever parses re-encodes to
//! something that parses back to the same packet.
#![no_main]

use fernsicht_proto::{InputHeader, MAX_DATAGRAM, Monitors, Packet, SealedHeader, VideoHeader};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(packet) = Packet::decode(data) else {
        return;
    };
    let mut buf = vec![0u8; MAX_DATAGRAM.max(data.len()) + VideoHeader::LEN];
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
        Packet::Input(h, body) => {
            let events: Vec<_> = InputHeader::events(body).collect();
            InputHeader::encode(h.session_id, &events, &mut buf)
        }
        Packet::InputAck(p) => p.encode(&mut buf),
        Packet::Audio(h, frame, previous) => h.encode(frame, previous, &mut buf),
        Packet::Handshake(h) => h.encode(&mut buf),
        Packet::Sealed(h, sealed) => {
            h.write(&mut buf);
            buf[SealedHeader::LEN..SealedHeader::LEN + sealed.len()].copy_from_slice(sealed);
            SealedHeader::LEN + sealed.len()
        }
        Packet::Pair(p) => p.encode(&mut buf),
        Packet::Reject(r) => r.encode(&mut buf),
        Packet::Discover(d) => d.encode(&mut buf),
        Packet::Announce(a) => a.encode(&mut buf),
        Packet::Monitors(m) => {
            let list: Vec<_> = m.iter().collect();
            Monitors::encode(m.session_id, m.current, &list, &mut buf)
        }
        Packet::SelectMonitor(p) => p.encode(&mut buf),
    };
    assert_eq!(Packet::decode(&buf[..n]), Ok(packet));
});
