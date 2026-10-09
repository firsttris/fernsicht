//! Datagrams → frames, with FEC recovery. Latest frame wins.

use fernsicht_proto::VideoHeader;
use reed_solomon_simd::ReedSolomonDecoder;

use crate::fec::MIN_RECOVERY_CAP;
use crate::frame_newer;

/// Frames tracked at once. Older frames are dropped when a newer one
/// completes or when a slot is needed.
pub const MAX_IN_FLIGHT: usize = 8;

/// A fully received frame; `data` borrows the reassembler's buffer.
#[derive(Debug)]
pub struct CompletedFrame<'a> {
    pub header: VideoHeader,
    pub data: &'a [u8],
    /// Client clock: arrival of the first packet of this frame.
    pub first_packet_us: u64,
    /// Client clock: the packet that completed the frame.
    pub completed_us: u64,
}

/// Receiver counters. [`Reassembler::take_interval`] returns and resets the
/// interval counters for feedback reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiverStats {
    pub packets_received: u32,
    pub packets_lost: u32,
    /// Data shards restored from recovery shards.
    pub packets_recovered: u32,
    pub packets_late: u32,
    pub frames_completed: u32,
    pub frames_dropped: u32,
    pub highest_frame_id: u32,
}

#[derive(Default)]
struct Group {
    seen: bool,
    data_shards: usize,
    recovery_shards: usize,
    offset: usize,
    have: Vec<bool>,
    data_have: usize,
    total_have: usize,
    done: bool,
    recovery: Vec<u8>,
}

impl Group {
    fn init(&mut self, h: &VideoHeader, shard: usize) {
        self.seen = true;
        self.data_shards = h.data_shards as usize;
        self.recovery_shards = h.recovery_shards as usize;
        self.offset = h.group_offset as usize;
        self.have.clear();
        self.have
            .resize(self.data_shards + self.recovery_shards, false);
        self.data_have = 0;
        self.total_have = 0;
        self.done = false;
        self.recovery.resize(self.recovery_shards * shard, 0);
    }

    fn matches(&self, h: &VideoHeader) -> bool {
        self.data_shards == h.data_shards as usize
            && self.recovery_shards == h.recovery_shards as usize
            && self.offset == h.group_offset as usize
    }
}

#[derive(Default)]
struct FrameSlot {
    active: bool,
    complete: bool,
    header: VideoHeader,
    shard_size: usize,
    buf: Vec<u8>,
    groups: Vec<Group>,
    groups_done: usize,
    received: usize,
    first_packet_us: u64,
}

impl FrameSlot {
    fn reset(&mut self, h: &VideoHeader, shard: usize, now_us: u64) {
        self.active = true;
        self.complete = false;
        self.header = *h;
        self.shard_size = shard;
        let padded = (h.frame_len as usize).div_ceil(shard) * shard;
        self.buf.resize(padded, 0);
        let groups = h.group_count as usize;
        if self.groups.len() < groups {
            self.groups.resize_with(groups, Group::default);
        }
        for g in &mut self.groups[..groups] {
            g.seen = false;
        }
        self.groups_done = 0;
        self.received = 0;
        self.first_packet_us = now_us;
    }

    fn matches(&self, h: &VideoHeader, shard: usize) -> bool {
        self.header.frame_len == h.frame_len
            && self.header.group_count == h.group_count
            && self.shard_size == shard
    }

    /// Packets we expected for this frame. Groups never seen are assumed to
    /// look like the largest seen group.
    fn expected_packets(&self) -> usize {
        let groups = &self.groups[..self.header.group_count as usize];
        let typical = groups
            .iter()
            .filter(|g| g.seen)
            .map(|g| g.data_shards + g.recovery_shards)
            .max()
            .unwrap_or(0);
        groups
            .iter()
            .map(|g| {
                if g.seen {
                    g.data_shards + g.recovery_shards
                } else {
                    typical
                }
            })
            .sum()
    }
}

pub struct Reassembler {
    slots: Vec<FrameSlot>,
    decoder: Option<ReedSolomonDecoder>,
    last_completed: Option<u32>,
    needs_keyframe: bool,
    session_id: Option<u32>,
    interval: ReceiverStats,
    total: ReceiverStats,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            slots: (0..MAX_IN_FLIGHT).map(|_| FrameSlot::default()).collect(),
            decoder: None,
            last_completed: None,
            needs_keyframe: true,
            session_id: None,
            interval: ReceiverStats::default(),
            total: ReceiverStats::default(),
        }
    }

    /// `true` until a keyframe arrives, and again after a frame was lost.
    pub fn needs_keyframe(&self) -> bool {
        self.needs_keyframe
    }

    pub fn totals(&self) -> ReceiverStats {
        self.total
    }

    /// Returns counters since the previous call and resets them.
    pub fn take_interval(&mut self) -> ReceiverStats {
        let mut s = std::mem::take(&mut self.interval);
        s.highest_frame_id = self.total.highest_frame_id;
        s
    }

    fn count(&mut self, f: impl Fn(&mut ReceiverStats)) {
        f(&mut self.interval);
        f(&mut self.total);
    }

    /// Feeds one video datagram (already parsed). Returns the frame if this
    /// packet completed it.
    pub fn push(
        &mut self,
        h: &VideoHeader,
        payload: &[u8],
        now_us: u64,
    ) -> Option<CompletedFrame<'_>> {
        match self.session_id {
            Some(id) if id != h.session_id => {
                // New session: forget everything from the old one.
                *self = Self::new();
                self.session_id = Some(h.session_id);
            }
            None => self.session_id = Some(h.session_id),
            _ => {}
        }
        self.count(|s| s.packets_received += 1);
        if self.interval.packets_received == 1
            || frame_newer(h.frame_id, self.total.highest_frame_id)
        {
            self.total.highest_frame_id = h.frame_id;
        }

        if let Some(last) = self.last_completed
            && !frame_newer(h.frame_id, last)
            && !self.has_slot(h.frame_id)
        {
            self.count(|s| s.packets_late += 1);
            return None;
        }

        let shard = payload.len();
        let idx = self.slot_for(h, shard, now_us)?;
        let slot = &mut self.slots[idx];
        if slot.complete {
            // Trailing shard of an already completed frame; still counts as
            // received for loss accounting.
            Self::mark(slot, h);
            return None;
        }
        let gi = h.group_index as usize;
        let shard_idx = h.shard_index as usize;
        {
            let buf_len = slot.buf.len();
            let g = &mut slot.groups[gi];
            if !g.seen {
                // The group's data shards must lie inside the frame buffer;
                // otherwise FEC recovery would write past its end. Recovery
                // shards are bounded by the data they protect, so a hostile
                // header can't make us buffer more than a few frames' worth.
                let end = h.group_offset as usize + h.data_shards as usize * shard;
                let max_recovery = h.data_shards.max(MIN_RECOVERY_CAP as u16);
                if end > buf_len || h.recovery_shards > max_recovery {
                    return None;
                }
                g.init(h, shard);
            } else if !g.matches(h) {
                return None;
            }
            if g.done || g.have[shard_idx] {
                Self::mark(slot, h);
                return None;
            }
        }
        if !Self::mark(slot, h) {
            return None;
        }
        let g = &mut slot.groups[gi];
        if shard_idx < g.data_shards {
            let start = g.offset + shard_idx * shard;
            slot.buf
                .get_mut(start..start + shard)?
                .copy_from_slice(payload);
            g.data_have += 1;
        } else {
            let r = shard_idx - g.data_shards;
            g.recovery[r * shard..(r + 1) * shard].copy_from_slice(payload);
        }
        g.total_have += 1;

        if g.total_have < g.data_shards {
            return None;
        }
        let restored = if g.data_have < g.data_shards {
            Self::recover(&mut self.decoder, slot, gi)?
        } else {
            0
        };
        self.count(|s| s.packets_recovered += restored as u32);
        let slot = &mut self.slots[idx];
        slot.groups[gi].done = true;
        slot.groups_done += 1;
        if slot.groups_done < slot.header.group_count as usize {
            return None;
        }
        Some(self.complete(idx, now_us))
    }

    /// Marks a shard as received; returns false for duplicates.
    fn mark(slot: &mut FrameSlot, h: &VideoHeader) -> bool {
        let g = &mut slot.groups[h.group_index as usize];
        if !g.seen || !g.matches(h) {
            return false;
        }
        let have = &mut g.have[h.shard_index as usize];
        if *have {
            return false;
        }
        *have = true;
        slot.received += 1;
        true
    }

    fn recover(
        decoder: &mut Option<ReedSolomonDecoder>,
        slot: &mut FrameSlot,
        gi: usize,
    ) -> Option<usize> {
        let shard = slot.shard_size;
        let g = &slot.groups[gi];
        let dec = match decoder {
            Some(d) => {
                d.reset(g.data_shards, g.recovery_shards, shard).ok()?;
                d
            }
            None => decoder
                .insert(ReedSolomonDecoder::new(g.data_shards, g.recovery_shards, shard).ok()?),
        };
        for i in 0..g.data_shards {
            if g.have[i] {
                let start = g.offset + i * shard;
                dec.add_original_shard(i, &slot.buf[start..start + shard])
                    .ok()?;
            }
        }
        for r in 0..g.recovery_shards {
            if g.have[g.data_shards + r] {
                dec.add_recovery_shard(r, &g.recovery[r * shard..(r + 1) * shard])
                    .ok()?;
            }
        }
        let result = dec.decode().ok()?;
        let mut restored = 0;
        for (i, data) in result.restored_original_iter() {
            let start = g.offset + i * shard;
            slot.buf[start..start + shard].copy_from_slice(data);
            restored += 1;
        }
        Some(restored)
    }

    fn complete(&mut self, idx: usize, now_us: u64) -> CompletedFrame<'_> {
        let frame_id = self.slots[idx].header.frame_id;
        let keyframe = self.slots[idx].header.keyframe;
        let gap = match self.last_completed {
            Some(last) => frame_id.wrapping_sub(last).wrapping_sub(1),
            // Frame ids start at 0 in every session.
            None => frame_id,
        };
        self.count(|s| {
            s.frames_completed += 1;
            s.frames_dropped += gap;
        });
        if keyframe {
            self.needs_keyframe = false;
        } else if gap > 0 {
            self.needs_keyframe = true;
        }
        self.last_completed = Some(frame_id);

        // Latest frame wins: anything older can no longer be shown.
        for i in 0..self.slots.len() {
            let s = &self.slots[i];
            if i != idx && s.active && !s.complete && !frame_newer(s.header.frame_id, frame_id) {
                self.retire(i);
            }
        }

        let slot = &mut self.slots[idx];
        slot.complete = true;
        CompletedFrame {
            header: slot.header,
            data: &slot.buf[..slot.header.frame_len as usize],
            first_packet_us: slot.first_packet_us,
            completed_us: now_us,
        }
    }

    /// Frees a slot and books its missing packets as lost.
    fn retire(&mut self, idx: usize) {
        let slot = &mut self.slots[idx];
        let lost = slot.expected_packets().saturating_sub(slot.received) as u32;
        slot.active = false;
        self.count(|s| s.packets_lost += lost);
    }

    fn has_slot(&self, frame_id: u32) -> bool {
        self.slots
            .iter()
            .any(|s| s.active && s.header.frame_id == frame_id)
    }

    fn slot_for(&mut self, h: &VideoHeader, shard: usize, now_us: u64) -> Option<usize> {
        if let Some(i) = self
            .slots
            .iter()
            .position(|s| s.active && s.header.frame_id == h.frame_id)
        {
            return self.slots[i].matches(h, shard).then_some(i);
        }
        // Every group carries at least one data shard of this frame.
        let shards_in_frame = (h.frame_len as usize).div_ceil(shard);
        if h.group_count as usize > shards_in_frame {
            return None;
        }
        let idx = match self.slots.iter().position(|s| !s.active) {
            Some(i) => i,
            None => {
                // Evict the oldest frame.
                let oldest = (0..self.slots.len())
                    .min_by(|&a, &b| {
                        let (fa, fb) =
                            (self.slots[a].header.frame_id, self.slots[b].header.frame_id);
                        if frame_newer(fa, fb) {
                            std::cmp::Ordering::Greater
                        } else if fa == fb {
                            std::cmp::Ordering::Equal
                        } else {
                            std::cmp::Ordering::Less
                        }
                    })
                    .unwrap();
                if frame_newer(self.slots[oldest].header.frame_id, h.frame_id) {
                    // Everything tracked is newer than this packet.
                    self.count(|s| s.packets_late += 1);
                    return None;
                }
                self.retire(oldest);
                oldest
            }
        };
        self.slots[idx].reset(h, shard, now_us);
        Some(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::{FecConfig, FrameMeta, Packetizer};
    use crate::loss::LossSim;
    use fernsicht_proto::Packet;

    fn packets(frame_id: u32, frame: &[u8], cfg: FecConfig, keyframe: bool) -> Vec<Vec<u8>> {
        let mut p = Packetizer::new(cfg).unwrap();
        let meta = FrameMeta {
            session_id: 1,
            frame_id,
            keyframe,
            ..FrameMeta::default()
        };
        p.packetize(&meta, frame).unwrap().to_vec()
    }

    fn feed(r: &mut Reassembler, pkt: &[u8]) -> Option<Vec<u8>> {
        let Packet::Video(h, payload) = Packet::decode(pkt).unwrap() else {
            panic!()
        };
        r.push(&h, payload, 0).map(|f| f.data.to_vec())
    }

    fn cfg(redundancy: f32) -> FecConfig {
        FecConfig {
            shard_size: 128,
            max_data_per_group: 16,
            redundancy,
            loss: 0.0,
            ..FecConfig::default()
        }
    }

    fn frame(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[test]
    fn reassembles_without_loss() {
        let f = frame(5_000, 1);
        let mut r = Reassembler::new();
        let pkts = packets(0, &f, cfg(0.2), true);
        let mut out = None;
        for p in &pkts {
            if let Some(done) = feed(&mut r, p) {
                out = Some(done);
            }
        }
        assert_eq!(out.unwrap(), f);
        assert!(!r.needs_keyframe());
        let s = r.take_interval();
        assert_eq!(s.frames_completed, 1);
        assert_eq!(s.packets_recovered, 0);
    }

    #[test]
    fn recovers_lost_data_shards() {
        let f = frame(16 * 128, 2); // exactly one group of 16
        let pkts = packets(0, &f, cfg(0.25), true); // 16 data + 4 recovery
        assert_eq!(pkts.len(), 20);
        let mut r = Reassembler::new();
        let mut out = None;
        for (i, p) in pkts.iter().enumerate() {
            if [0, 5, 9, 15].contains(&i) {
                continue;
            }
            if let Some(done) = feed(&mut r, p) {
                out = Some(done);
            }
        }
        assert_eq!(out.unwrap(), f);
        assert_eq!(r.take_interval().packets_recovered, 4);
    }

    #[test]
    fn too_much_loss_drops_frame_and_requests_keyframe() {
        let mut r = Reassembler::new();
        let f0 = frame(1_000, 0);
        for p in &packets(0, &f0, cfg(0.1), true) {
            feed(&mut r, p);
        }
        assert!(!r.needs_keyframe());
        // frame 1: lose two data shards with only one recovery shard
        let f1 = frame(1_000, 1);
        for (i, p) in packets(1, &f1, cfg(0.1), false).iter().enumerate() {
            if i == 0 || i == 1 {
                continue;
            }
            assert!(feed(&mut r, p).is_none());
        }
        // frame 2 completes; frame 1 counts as dropped
        let f2 = frame(1_000, 2);
        let mut out = None;
        for p in &packets(2, &f2, cfg(0.1), false) {
            if let Some(d) = feed(&mut r, p) {
                out = Some(d);
            }
        }
        assert_eq!(out.unwrap(), f2);
        assert!(r.needs_keyframe());
        let s = r.take_interval();
        assert_eq!(s.frames_completed, 2);
        assert_eq!(s.frames_dropped, 1);
        assert_eq!(s.packets_lost, 2);
    }

    #[test]
    fn late_packets_of_older_frames_are_ignored() {
        let mut r = Reassembler::new();
        let old = packets(0, &frame(500, 0), cfg(0.0), true);
        let new = packets(1, &frame(500, 1), cfg(0.0), false);
        for p in &new {
            feed(&mut r, p);
        }
        for p in &old {
            assert!(feed(&mut r, p).is_none());
        }
        assert_eq!(r.totals().frames_completed, 1);
        assert!(r.totals().packets_late > 0);
    }

    #[test]
    fn duplicates_are_harmless() {
        let mut r = Reassembler::new();
        let f = frame(2_000, 3);
        let pkts = packets(0, &f, cfg(0.2), true);
        let mut completions = 0;
        for p in pkts.iter().chain(pkts.iter()) {
            if feed(&mut r, p).is_some() {
                completions += 1;
            }
        }
        assert_eq!(completions, 1);
    }

    #[test]
    fn reordered_packets_complete() {
        let mut r = Reassembler::new();
        let f = frame(9_000, 4);
        let mut pkts = packets(0, &f, cfg(0.2), true);
        pkts.reverse();
        let mut out = None;
        for p in &pkts {
            if let Some(d) = feed(&mut r, p) {
                out = Some(d);
            }
        }
        assert_eq!(out.unwrap(), f);
    }

    #[test]
    fn one_percent_loss_is_fully_recovered() {
        // Phase 1 acceptance criterion: 1 % random loss without visible
        // artefacts, i.e. no dropped frames with the default FEC sizing.
        let mut loss = LossSim::new(0.01, 0xC0FFEE);
        let mut r = Reassembler::new();
        let fec = FecConfig::default();
        let mut p = Packetizer::new(fec).unwrap();
        let mut completed = 0;
        for id in 0..600u32 {
            let f = frame(if id % 60 == 0 { 200_000 } else { 42_000 }, id as u8);
            let meta = FrameMeta {
                session_id: 1,
                frame_id: id,
                keyframe: id % 60 == 0,
                ..FrameMeta::default()
            };
            for pkt in p.packetize(&meta, &f).unwrap() {
                if loss.drop_packet() {
                    continue;
                }
                let Packet::Video(h, payload) = Packet::decode(pkt).unwrap() else {
                    panic!()
                };
                if let Some(done) = r.push(&h, payload, 0) {
                    assert_eq!(done.data, &f[..]);
                    completed += 1;
                }
            }
        }
        let t = r.totals();
        assert_eq!(completed, 600, "{t:?}");
        assert_eq!(t.frames_dropped, 0);
        assert!(t.packets_recovered > 0);
    }

    #[test]
    fn frames_before_the_first_completed_one_count_as_dropped() {
        let mut r = Reassembler::new();
        for p in &packets(3, &frame(500, 3), cfg(0.0), true) {
            feed(&mut r, p);
        }
        let s = r.take_interval();
        assert_eq!((s.frames_completed, s.frames_dropped), (1, 3));
    }

    #[test]
    fn group_outside_frame_is_rejected() {
        // Regression (found by proptest): a group whose data shards extend
        // past frame_len made FEC recovery write out of bounds.
        let mut r = Reassembler::new();
        let h = VideoHeader {
            session_id: 1,
            frame_len: 160,
            group_count: 1,
            group_offset: 100,
            data_shards: 2,
            recovery_shards: 1,
            shard_index: 2,
            slice_count: 1,
            ..VideoHeader::default()
        };
        assert!(r.push(&h, &[0; 40], 0).is_none());
        let h = VideoHeader {
            shard_index: 0,
            ..h
        };
        assert!(r.push(&h, &[0; 40], 0).is_none());
    }

    #[test]
    fn hostile_group_geometry_is_rejected() {
        let mut r = Reassembler::new();
        let base = VideoHeader {
            session_id: 1,
            frame_len: 1_000,
            group_count: 1,
            data_shards: 4,
            recovery_shards: 1,
            slice_count: 1,
            ..VideoHeader::default()
        };
        // More groups than the frame has shards.
        let many_groups = VideoHeader {
            group_count: 100,
            ..base
        };
        assert!(r.push(&many_groups, &[0; 100], 0).is_none());
        // Far more recovery than data.
        let greedy = VideoHeader {
            recovery_shards: 900,
            shard_index: 500,
            ..base
        };
        assert!(r.push(&greedy, &[0; 100], 0).is_none());
        assert_eq!(r.totals().frames_completed, 0);
    }

    #[test]
    fn new_session_resets_state() {
        let mut r = Reassembler::new();
        for p in &packets(100, &frame(300, 0), cfg(0.0), true) {
            feed(&mut r, p);
        }
        let mut pk = Packetizer::new(cfg(0.0)).unwrap();
        let meta = FrameMeta {
            session_id: 2,
            frame_id: 0,
            keyframe: true,
            ..FrameMeta::default()
        };
        let pkts = pk.packetize(&meta, &frame(300, 9)).unwrap().to_vec();
        let mut done = false;
        for p in &pkts {
            done |= feed(&mut r, p).is_some();
        }
        assert!(done, "frame 0 of a new session must not count as late");
    }
}
