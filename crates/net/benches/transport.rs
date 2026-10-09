//! Hot-path benchmarks: packetize + FEC, and reassembly with and without
//! recovery. Run with `cargo bench -p fernsicht-net`.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use fernsicht_net::{FecConfig, FrameMeta, Packetizer, Reassembler};
use fernsicht_proto::Packet;

/// P-frame at 20 Mbit/s and 60 fps, and a 6× keyframe.
const SIZES: [(&str, usize); 2] = [("p-frame 42 kB", 42_000), ("keyframe 250 kB", 250_000)];

fn meta(frame_id: u32) -> FrameMeta {
    FrameMeta {
        session_id: 1,
        frame_id,
        keyframe: true,
        ..FrameMeta::default()
    }
}

fn packetize(c: &mut Criterion) {
    let mut g = c.benchmark_group("packetize");
    for (name, len) in SIZES {
        let frame = vec![0x5Au8; len];
        let mut p = Packetizer::new(FecConfig::default()).unwrap();
        g.throughput(Throughput::Bytes(len as u64));
        g.bench_with_input(BenchmarkId::from_parameter(name), &frame, |b, f| {
            b.iter(|| black_box(p.packetize(&meta(0), f).unwrap().len()));
        });
    }
    g.finish();
}

fn reassemble(c: &mut Criterion) {
    let mut g = c.benchmark_group("reassemble");
    for (name, len) in SIZES {
        let frame = vec![0xA5u8; len];
        let mut p = Packetizer::new(FecConfig::default()).unwrap();
        let pkts: Vec<Vec<u8>> = p.packetize(&meta(0), &frame).unwrap().to_vec();
        // Lose the first data shard of every group: forces RS recovery.
        let lossy: Vec<Vec<u8>> = pkts
            .iter()
            .filter(
                |pkt| !matches!(Packet::decode(pkt), Ok(Packet::Video(h, _)) if h.shard_index == 0),
            )
            .cloned()
            .collect();
        g.throughput(Throughput::Bytes(len as u64));
        for (label, input) in [("clean", &pkts), ("recovery", &lossy)] {
            g.bench_with_input(BenchmarkId::new(label, name), input, |b, input| {
                let mut r = Reassembler::new();
                let mut id = 0u32;
                b.iter(|| {
                    // Fresh frame id each round so the frame is new to the reassembler.
                    id = id.wrapping_add(1);
                    let mut done = false;
                    for pkt in input.iter() {
                        let Ok(Packet::Video(mut h, payload)) = Packet::decode(pkt) else {
                            unreachable!()
                        };
                        h.frame_id = id;
                        done |= r.push(&h, payload, 0).is_some();
                    }
                    assert!(done);
                });
            });
        }
    }
    g.finish();
}

fn decode(c: &mut Criterion) {
    let mut p = Packetizer::new(FecConfig::default()).unwrap();
    let pkt = p.packetize(&meta(0), &[1u8; 2000]).unwrap()[0].clone();
    c.bench_function("proto decode video", |b| {
        b.iter(|| black_box(Packet::decode(black_box(&pkt))))
    });
}

criterion_group!(benches, packetize, reassemble, decode);
criterion_main!(benches);
