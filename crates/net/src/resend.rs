//! What the host keeps to answer NACKs (C2): the last few frames' packets,
//! already marked as retransmissions.
//!
//! A NACK is worth answering only while the frame can still be shown, a
//! frame interval or so after it was sent; [`KEPT_FRAMES`] covers that with
//! room. The buffers are reused frame after frame: keeping a frame copies
//! its packets (a memcpy of the encoded frame, a few µs for a P-frame) and
//! allocates nothing once they have grown.

use fernsicht_proto::{Nack, Packet, ShardRef, VideoHeader};

/// Frames whose packets can be sent again.
pub const KEPT_FRAMES: usize = 4;

#[derive(Debug, Default)]
struct Kept {
    frame_id: Option<u32>,
    /// Plaintext packets, flagged `retransmit`; the first `used` are this
    /// frame's.
    packets: Vec<Vec<u8>>,
    used: usize,
    /// Which shard each packet carries.
    shards: Vec<ShardRef>,
}

#[derive(Debug, Default)]
pub struct ResendStore {
    frames: [Kept; KEPT_FRAMES],
    next: usize,
}

impl ResendStore {
    /// Keeps frame `frame_id`'s packets, in place of the oldest frame's.
    pub fn keep<P: AsRef<[u8]>>(&mut self, frame_id: u32, packets: &[P]) {
        let k = &mut self.frames[self.next];
        self.next = (self.next + 1) % KEPT_FRAMES;
        if k.packets.len() < packets.len() {
            k.packets.resize_with(packets.len(), Vec::new);
        }
        k.shards.clear();
        k.used = 0;
        for (dst, src) in k.packets.iter_mut().zip(packets) {
            let src = src.as_ref();
            let Ok(Packet::Video(h, _)) = Packet::decode(src) else {
                continue;
            };
            dst.clear();
            dst.extend_from_slice(src);
            VideoHeader::mark_retransmit(dst);
            k.shards.push(ShardRef {
                group: h.group_index,
                shard: h.shard_index,
            });
            k.used += 1;
        }
        k.frame_id = Some(frame_id);
    }

    /// The packets `nack` asks for that are still kept (none once the
    /// frame is older than [`KEPT_FRAMES`]).
    pub fn find<'a>(&'a self, nack: &'a Nack) -> impl Iterator<Item = &'a [u8]> + 'a {
        let kept = self
            .frames
            .iter()
            .find(|k| k.frame_id == Some(nack.frame_id));
        nack.shards().iter().filter_map(move |want| {
            let k = kept?;
            let i = k.shards.iter().position(|s| s == want)?;
            Some(k.packets[i].as_slice())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::{FecConfig, FrameMeta, Packetizer};

    fn frame(p: &mut Packetizer, frame_id: u32) -> Vec<Vec<u8>> {
        let meta = FrameMeta {
            session_id: 7,
            frame_id,
            keyframe: false,
            capture_us: 0,
            capture_ready_delta_us: 0,
            encoded_delta_us: 0,
        };
        p.packetize(&meta, &vec![frame_id as u8; 20_000])
            .unwrap()
            .to_vec()
    }

    fn header(packet: &[u8]) -> VideoHeader {
        match Packet::decode(packet) {
            Ok(Packet::Video(h, _)) => h,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_shards_asked_for_go_again_marked_as_such() {
        let mut p = Packetizer::new(FecConfig::default()).unwrap();
        let mut store = ResendStore::default();
        let sent = frame(&mut p, 3);
        store.keep(3, &sent);
        let want = [
            ShardRef { group: 0, shard: 2 },
            ShardRef { group: 0, shard: 5 },
        ];
        let nack = Nack::new(7, 3, &want);
        let again: Vec<&[u8]> = store.find(&nack).collect();
        assert_eq!(again.len(), 2);
        for (packet, w) in again.iter().zip(want) {
            let h = header(packet);
            assert!(h.retransmit);
            assert_eq!(
                (h.frame_id, h.group_index, h.shard_index),
                (3, w.group, w.shard)
            );
            let original = &sent[usize::from(w.shard)];
            assert_eq!(&packet[VideoHeader::LEN..], &original[VideoHeader::LEN..]);
        }
        assert!(!header(&sent[2]).retransmit, "the originals stay as sent");
    }

    #[test]
    fn only_the_last_frames_are_kept() {
        let mut p = Packetizer::new(FecConfig::default()).unwrap();
        let mut store = ResendStore::default();
        for id in 0..KEPT_FRAMES as u32 + 2 {
            store.keep(id, &frame(&mut p, id));
        }
        let ask = |id| Nack::new(7, id, &[ShardRef { group: 0, shard: 0 }]);
        assert_eq!(store.find(&ask(0)).count(), 0, "too old");
        assert_eq!(store.find(&ask(1)).count(), 0, "too old");
        assert_eq!(store.find(&ask(2)).count(), 1);
        assert_eq!(store.find(&ask(KEPT_FRAMES as u32 + 1)).count(), 1);
        let beyond = Nack::new(7, 2, &[ShardRef { group: 9, shard: 0 }]);
        assert_eq!(store.find(&beyond).count(), 0, "no such shard");
    }
}
