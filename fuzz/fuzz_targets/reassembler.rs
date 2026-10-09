//! A hostile stream of datagrams (length-prefixed in the input) must never
//! crash the reassembler or produce a frame of the wrong length.
#![no_main]

use fernsicht_net::Reassembler;
use fernsicht_proto::Packet;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut r = Reassembler::new();
    let mut rest = data;
    let mut now = 0u64;
    while let [len, tail @ ..] = rest {
        let len = (*len as usize).min(tail.len());
        let (datagram, next) = tail.split_at(len);
        rest = next;
        now += 100;
        if let Ok(Packet::Video(h, payload)) = Packet::decode(datagram)
            && let Some(f) = r.push(&h, payload, now)
        {
            assert_eq!(f.data.len(), f.header.frame_len as usize);
        }
        let _ = r.take_interval();
    }
});
