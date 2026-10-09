//! Property tests for FEC, reassembly, pacing and clock sync.

use fernsicht_net::clock_sync::ClockSync;
use fernsicht_net::fec::{MIN_RECOVERY_CAP, TARGET_GROUP_FAILURE, group_failure};
use fernsicht_net::{FecConfig, FrameMeta, LossEstimator, Pacer, Packetizer, Reassembler};
use fernsicht_proto::{ClockPong, Packet, VideoHeader};
use proptest::prelude::*;

fn fec_config() -> impl Strategy<Value = FecConfig> {
    (1usize..=64, 1usize..=48, 0.0f32..0.5, 0.0f32..0.2).prop_map(
        |(half_shard, group, redundancy, loss)| FecConfig {
            shard_size: half_shard * 2 * 8,
            max_data_per_group: group,
            redundancy,
            loss,
            ..FecConfig::default()
        },
    )
}

fn frame(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(37) ^ seed)
        .collect()
}

/// Parsed (header, payload) pairs for one frame.
fn shards(cfg: FecConfig, meta: FrameMeta, data: &[u8]) -> Vec<(VideoHeader, Vec<u8>)> {
    let mut p = Packetizer::new(cfg).unwrap();
    p.packetize(&meta, data)
        .unwrap()
        .iter()
        .map(|pkt| match Packet::decode(pkt).unwrap() {
            Packet::Video(h, payload) => (h, payload.to_vec()),
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

fn meta(frame_id: u32) -> FrameMeta {
    FrameMeta {
        session_id: 1,
        frame_id,
        keyframe: frame_id == 0,
        ..FrameMeta::default()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Any loss pattern within each group's recovery budget, in any order,
    /// reconstructs the exact frame.
    #[test]
    fn recoverable_loss_reconstructs_exactly(
        cfg in fec_config(),
        len in 1usize..40_000,
        seed: u8,
        drop_bits: Vec<bool>,
        order in any::<proptest::sample::Index>(),
        rotate: usize,
    ) {
        let data = frame(len, seed);
        let mut pkts = shards(cfg, meta(0), &data);

        // Drop up to `recovery_shards` packets per group.
        let mut dropped_per_group = std::collections::HashMap::<u16, u16>::new();
        let mut kept = Vec::new();
        for (i, (h, payload)) in pkts.drain(..).enumerate() {
            let budget = dropped_per_group.entry(h.group_index).or_default();
            if drop_bits.get(i).copied().unwrap_or(false) && *budget < h.recovery_shards {
                *budget += 1;
                continue;
            }
            kept.push((h, payload));
        }
        // Deterministic shuffle: rotate and interleave.
        let n = kept.len();
        kept.rotate_left(rotate % n.max(1));
        let start = order.index(n.max(1));
        kept.swap(0, start.min(n.saturating_sub(1)));

        let mut r = Reassembler::new();
        let mut out = None;
        for (h, payload) in &kept {
            if let Some(f) = r.push(h, payload, 0) {
                prop_assert!(out.is_none(), "frame completed twice");
                out = Some(f.data.to_vec());
            }
        }
        prop_assert_eq!(out.as_deref(), Some(&data[..]));
    }

    /// Losing more than a group can repair never yields a frame, and never
    /// wrong data.
    #[test]
    fn unrecoverable_loss_never_completes(
        cfg in fec_config(),
        len in 1usize..20_000,
        victim: proptest::sample::Index,
    ) {
        let data = frame(len, 7);
        let pkts = shards(cfg, meta(0), &data);
        let groups = pkts[0].0.group_count;
        let g = (victim.index(groups as usize)) as u16;
        let mut to_drop = pkts.iter().find(|(h, _)| h.group_index == g).unwrap().0.recovery_shards + 1;
        let mut r = Reassembler::new();
        for (h, payload) in &pkts {
            if h.group_index == g && to_drop > 0 {
                to_drop -= 1;
                continue;
            }
            prop_assert!(r.push(h, payload, 0).is_none());
        }
    }

    /// Arbitrary (validated) headers with arbitrary payloads never panic the
    /// reassembler and never produce a frame of the wrong length.
    #[test]
    fn hostile_packets_are_safe(packets in proptest::collection::vec(
        (0u32..6, 1u32..5_000, 1u16..4, 0u16..4, 1u16..8, 0u16..4, 0u16..12, 1usize..64, 0u32..5_000),
        0..200,
    )) {
        let mut r = Reassembler::new();
        for (frame_id, frame_len, group_count, gi, data, rec, si, half, offset) in packets {
            let h = VideoHeader {
                session_id: 1,
                frame_id,
                frame_len,
                group_count,
                group_index: gi % group_count,
                group_offset: offset % frame_len,
                data_shards: data,
                recovery_shards: rec,
                shard_index: si % (data + rec),
                slice_count: 1,
                ..VideoHeader::default()
            };
            let payload = vec![0xAB; half * 2];
            if let Some(f) = r.push(&h, &payload, 0) {
                prop_assert_eq!(f.data.len(), f.header.frame_len as usize);
            }
        }
    }

    #[test]
    fn loss_estimate_stays_in_bounds(losses in proptest::collection::vec(-1.0f32..2.0, 1..100)) {
        let mut e = LossEstimator::default();
        for l in losses {
            let est = e.update(l);
            prop_assert!((LossEstimator::FLOOR..=1.0).contains(&est));
        }
    }

    /// Recovery count: at least the floor ratio, at most the cap, and
    /// meets the failure target whenever the cap allows it.
    #[test]
    fn recovery_count_is_sized_correctly(cfg in fec_config(), data in 1usize..300) {
        let r = cfg.recovery_for(data);
        if cfg.redundancy <= 0.0 {
            prop_assert_eq!(r, 0);
            return Ok(());
        }
        let floor = ((data as f32 * cfg.redundancy).ceil() as usize).max(1);
        let cap = ((data as f32 * cfg.max_redundancy).ceil() as usize)
            .max(floor)
            .max(MIN_RECOVERY_CAP);
        prop_assert!((floor..=cap).contains(&r));
        if cfg.loss > 0.0 && r < cap {
            prop_assert!(group_failure(data, r, f64::from(cfg.loss)) <= TARGET_GROUP_FAILURE);
        }
    }

    #[test]
    fn group_failure_is_a_probability_and_monotonic(d in 1usize..100, r in 0usize..50, p in 0.0f64..0.5) {
        let f = group_failure(d, r, p);
        prop_assert!((0.0..=1.0).contains(&f));
        prop_assert!(group_failure(d, r + 1, p) <= f + 1e-12);
    }

    /// Pacing offsets never decrease and stay inside the budget.
    #[test]
    fn pacer_schedule_is_monotonic(rate in 1u64..1_000_000_000, n in 1usize..500,
                                   size in 1usize..1500, budget in 0u64..50_000) {
        let p = Pacer { rate_bytes_per_sec: rate, burst: 1 };
        let mut prev = 0;
        for i in 0..n {
            let off = p.offset_us(i, i * size, n, budget);
            prop_assert!(off >= prev);
            prop_assert!(off <= budget);
            prev = off;
        }
    }

    /// With a symmetric path the estimate is exact, whatever the offset.
    #[test]
    fn clock_sync_exact_on_symmetric_path(offset in -10_000_000_000i64..10_000_000_000,
                                          one_way in 0u64..100_000, turnaround in 0u64..10_000,
                                          t0 in 20_000_000_000u64..30_000_000_000) {
        let t1 = (t0 + one_way) as i64 + offset;
        let t2 = t1 + turnaround as i64;
        let t3 = t0 + 2 * one_way + turnaround;
        let mut c = ClockSync::new();
        let s = c.on_pong(&ClockPong { seq: 0, client_send_us: t0, host_recv_us: t1 as u64, host_send_us: t2 as u64 }, t3);
        prop_assert_eq!(s.offset_us, offset);
        prop_assert_eq!(s.rtt_us, 2 * one_way);
        prop_assert_eq!(c.host_to_client(t2 as u64), t0 as i64 + one_way as i64 + turnaround as i64);
    }

    /// Asymmetry shifts the estimate by at most half the RTT.
    #[test]
    fn clock_sync_error_bounded_by_half_rtt(up in 0u64..50_000, down in 0u64..50_000,
                                            offset in -1_000_000i64..1_000_000) {
        let t0 = 10_000_000u64;
        let t1 = (t0 + up) as i64 + offset;
        let t3 = t0 + up + down;
        let mut c = ClockSync::new();
        let s = c.on_pong(&ClockPong { seq: 0, client_send_us: t0, host_recv_us: t1 as u64, host_send_us: t1 as u64 }, t3);
        prop_assert!((s.offset_us - offset).unsigned_abs() <= s.rtt_us / 2 + 1);
    }
}
