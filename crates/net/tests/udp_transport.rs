//! Integration: packetizer → pacer → real UDP socket → reassembler.

use std::thread;
use std::time::Duration;

use fernsicht_net::socket::bind_udp;
use fernsicht_net::{FecConfig, FrameMeta, LossSim, Pacer, Packetizer, Reassembler};
use fernsicht_proto::Packet;

fn frame(id: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32 ^ id.wrapping_mul(2654435761)) as u8)
        .collect()
}

fn len_for(id: u32) -> usize {
    if id.is_multiple_of(30) {
        120_000
    } else {
        20_000 + (id as usize * 997) % 15_000
    }
}

/// Sends `frames` frames, dropping packets with probability `loss` at the
/// sender, and returns (completed frame count, receiver totals).
fn transfer(frames: u32, loss: f64, redundancy: f32) -> (u32, fernsicht_net::ReceiverStats) {
    let rx = bind_udp("127.0.0.1:0").unwrap();
    rx.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let tx = bind_udp("127.0.0.1:0").unwrap();
    tx.connect(rx.local_addr().unwrap()).unwrap();

    let sender = thread::spawn(move || {
        let mut p = Packetizer::new(FecConfig {
            redundancy,
            loss: 0.0,
            ..FecConfig::default()
        })
        .unwrap();
        let pacer = Pacer::default();
        let mut l = LossSim::new(loss, 99);
        for id in 0..frames {
            let meta = FrameMeta {
                session_id: 3,
                frame_id: id,
                keyframe: id % 30 == 0,
                ..FrameMeta::default()
            };
            let pkts = p.packetize(&meta, &frame(id, len_for(id))).unwrap();
            pacer
                .pace(pkts, Duration::from_millis(4), |pkt| {
                    if !l.drop_packet() {
                        tx.send(pkt)?;
                    }
                    Ok::<_, std::io::Error>(())
                })
                .unwrap();
            thread::sleep(Duration::from_millis(2));
        }
    });

    let mut r = Reassembler::new();
    let mut buf = [0u8; 2048];
    let mut completed = 0;
    while let Ok(n) = rx.recv(&mut buf) {
        let Ok(Packet::Video(h, payload)) = Packet::decode(&buf[..n]) else {
            panic!("bad packet")
        };
        if let Some(f) = r.push(&h, payload, 0) {
            assert_eq!(
                f.data,
                &frame(f.header.frame_id, len_for(f.header.frame_id))[..]
            );
            completed += 1;
        }
    }
    sender.join().unwrap();
    (completed, r.totals())
}

#[test]
fn lossless_transfer_delivers_every_frame() {
    let (completed, s) = transfer(120, 0.0, 0.1);
    assert_eq!(completed, 120, "{s:?}");
    assert_eq!(s.frames_dropped, 0);
    assert_eq!(s.packets_recovered, 0);
}

#[test]
fn lossy_transfer_is_repaired_by_fec() {
    let (completed, s) = transfer(120, 0.02, 0.25);
    assert_eq!(completed, 120, "{s:?}");
    assert!(s.packets_recovered > 0, "{s:?}");
    assert!(s.packets_lost > 0, "{s:?}");
}

#[test]
fn without_fec_loss_drops_frames_but_never_corrupts() {
    let (completed, s) = transfer(120, 0.02, 0.0);
    // Every completed frame was verified byte-for-byte inside `transfer`.
    assert!(completed < 120, "{s:?}");
    assert!(s.frames_dropped > 0, "{s:?}");
}
