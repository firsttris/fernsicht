//! Packetize an arbitrary frame, lose an arbitrary subset of packets within
//! each group's recovery budget, deliver the rest in a scrambled order: the
//! exact frame must come out.
#![no_main]

use fernsicht_net::{FecConfig, FrameMeta, Packetizer, Reassembler};
use fernsicht_proto::Packet;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [shard, group, ratio, seed, frame @ ..] = data else {
        return;
    };
    if frame.is_empty() {
        return;
    }
    let cfg = FecConfig {
        shard_size: (usize::from(*shard % 64) + 1) * 16,
        max_data_per_group: usize::from(*group % 48) + 1,
        redundancy: f32::from(*ratio % 50 + 1) / 100.0,
        ..FecConfig::default()
    };
    let mut p = Packetizer::new(cfg).unwrap();
    let meta = FrameMeta {
        session_id: 1,
        frame_id: 0,
        keyframe: true,
        ..FrameMeta::default()
    };
    let mut pkts: Vec<Vec<u8>> = p.packetize(&meta, frame).unwrap().to_vec();

    // Drop within budget, chosen by the seed bits.
    let mut budget = std::collections::HashMap::new();
    let mut bits = u64::from(*seed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    pkts.retain(|pkt| {
        let Ok(Packet::Video(h, _)) = Packet::decode(pkt) else {
            unreachable!()
        };
        bits = bits.rotate_left(7) ^ 0xA5;
        let used = budget.entry(h.group_index).or_insert(0u16);
        if bits & 3 == 0 && *used < h.recovery_shards {
            *used += 1;
            false
        } else {
            true
        }
    });
    let n = pkts.len();
    pkts.rotate_left(usize::from(*seed) % n.max(1));

    let mut r = Reassembler::new();
    let mut out = None;
    for pkt in &pkts {
        let Ok(Packet::Video(h, payload)) = Packet::decode(pkt) else {
            unreachable!()
        };
        if let Some(f) = r.push(&h, payload, 0) {
            assert!(out.is_none());
            out = Some(f.data.to_vec());
        }
    }
    assert_eq!(out.as_deref(), Some(frame));
});
