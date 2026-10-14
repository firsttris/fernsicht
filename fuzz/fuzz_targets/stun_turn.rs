//! STUN and TURN messages from anyone on the internet: the parser, its
//! attribute readers and integrity checks, and the TURN client fed with
//! them, never panic.
#![no_main]

use std::time::Instant;

use fernsicht_internet::stun::{self, Message, attr};
use fernsicht_internet::turn::{Credentials, TurnClient};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = Message::parse(data) {
        let _ = m.attributes().count();
        let _ = (
            m.reflexive_address(),
            m.error_code(),
            m.u32_attr(attr::LIFETIME),
            m.text(attr::NONCE),
            m.xor_address(attr::XOR_RELAYED_ADDRESS),
            m.check_integrity(b"key"),
            m.check_fingerprint(),
        );
    }
    let _ = stun::parse_channel_data(data);
    let now = Instant::now();
    let credentials = Credentials {
        username: "u".into(),
        password: "p".into(),
    };
    let mut turn = TurnClient::new("192.0.2.1:3478".parse().unwrap(), credentials, [0; 8], now);
    // The first request's transaction, so that an answer can match it.
    if let Some(first) = turn.poll_transmit()
        && data.len() >= 20
    {
        let mut answer = data.to_vec();
        answer[8..20].copy_from_slice(&first[8..20]);
        let _ = turn.handle(&answer, now);
    }
    let _ = turn.handle(data, now);
    turn.tick(now);
    let mut out = [0u8; 64];
    let _ = turn.wrap("192.0.2.9:1".parse().unwrap(), data, &mut out);
});
