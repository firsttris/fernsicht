//! Frame → FEC-protected datagrams.

use fernsicht_proto::{DEFAULT_SHARD_SIZE, VideoHeader};
use reed_solomon_simd::ReedSolomonEncoder;
use thiserror::Error;

#[derive(Clone, Copy, Debug)]
pub struct FecConfig {
    /// Payload bytes per datagram. Must be even.
    pub shard_size: usize,
    /// Upper bound for data shards per FEC group. Smaller groups decode
    /// faster and isolate burst loss; larger groups protect better.
    pub max_data_per_group: usize,
    /// Recovery shards as a fraction of data shards (0.0 disables FEC).
    pub redundancy: f32,
}

impl Default for FecConfig {
    fn default() -> Self {
        Self {
            shard_size: DEFAULT_SHARD_SIZE,
            max_data_per_group: 64,
            redundancy: 0.10,
        }
    }
}

impl FecConfig {
    pub fn recovery_for(&self, data_shards: usize) -> usize {
        if self.redundancy <= 0.0 {
            0
        } else {
            ((data_shards as f32 * self.redundancy).ceil() as usize).max(1)
        }
    }
}

/// Per-frame values copied into every packet header.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameMeta {
    pub session_id: u32,
    pub frame_id: u32,
    pub keyframe: bool,
    pub capture_us: u64,
    pub capture_ready_delta_us: u32,
    pub encoded_delta_us: u32,
}

#[derive(Debug, Error)]
pub enum FecError {
    #[error("empty frame")]
    Empty,
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
    #[error("invalid FEC config: {0}")]
    Config(&'static str),
    #[error("reed-solomon: {0}")]
    ReedSolomon(#[from] reed_solomon_simd::Error),
}

/// Turns encoded frames into ready-to-send datagrams.
///
/// Packet buffers and the Reed-Solomon encoder are reused across frames,
/// so after warm-up packetizing does not allocate.
pub struct Packetizer {
    cfg: FecConfig,
    encoder: Option<ReedSolomonEncoder>,
    packets: Vec<Vec<u8>>,
    len: usize,
}

impl Packetizer {
    pub fn new(cfg: FecConfig) -> Result<Self, FecError> {
        if cfg.shard_size == 0 || cfg.shard_size % 2 != 0 {
            return Err(FecError::Config("shard_size must be even and non-zero"));
        }
        if cfg.max_data_per_group == 0 || cfg.max_data_per_group > 4096 {
            return Err(FecError::Config("max_data_per_group must be 1..=4096"));
        }
        Ok(Self {
            cfg,
            encoder: None,
            packets: Vec::new(),
            len: 0,
        })
    }

    pub fn config(&self) -> FecConfig {
        self.cfg
    }

    /// Changes redundancy for subsequent frames (adaptive FEC).
    pub fn set_redundancy(&mut self, redundancy: f32) {
        self.cfg.redundancy = redundancy.clamp(0.0, 1.0);
    }

    /// Splits `frame` into datagrams. The returned packets stay valid until
    /// the next call.
    pub fn packetize(&mut self, meta: &FrameMeta, frame: &[u8]) -> Result<&[Vec<u8>], FecError> {
        if frame.is_empty() {
            return Err(FecError::Empty);
        }
        if frame.len() > u32::MAX as usize {
            return Err(FecError::TooLarge(frame.len()));
        }
        let shard = self.cfg.shard_size;
        let data_total = frame.len().div_ceil(shard);
        let group_count = data_total.div_ceil(self.cfg.max_data_per_group);
        if group_count > u16::MAX as usize {
            return Err(FecError::TooLarge(frame.len()));
        }

        self.len = 0;
        let mut header = VideoHeader {
            keyframe: meta.keyframe,
            session_id: meta.session_id,
            frame_id: meta.frame_id,
            frame_len: frame.len() as u32,
            capture_us: meta.capture_us,
            capture_ready_delta_us: meta.capture_ready_delta_us,
            encoded_delta_us: meta.encoded_delta_us,
            group_count: group_count as u16,
            slice_index: 0,
            slice_count: 1,
            ..VideoHeader::default()
        };

        for group in 0..group_count {
            let first_shard = group * self.cfg.max_data_per_group;
            let data = (data_total - first_shard).min(self.cfg.max_data_per_group);
            let recovery = self.cfg.recovery_for(data);
            let offset = first_shard * shard;
            header.group_index = group as u16;
            header.group_offset = offset as u32;
            header.data_shards = data as u16;
            header.recovery_shards = recovery as u16;

            let group_start = self.len;
            for i in 0..data {
                let start = offset + i * shard;
                let end = (start + shard).min(frame.len());
                header.shard_index = i as u16;
                let pkt = self.next_packet();
                header.write(&mut pkt[..VideoHeader::LEN]);
                let payload = &mut pkt[VideoHeader::LEN..];
                payload[..end - start].copy_from_slice(&frame[start..end]);
                payload[end - start..].fill(0);
            }

            if recovery == 0 {
                continue;
            }
            let encoder = match &mut self.encoder {
                Some(enc) => {
                    enc.reset(data, recovery, shard)?;
                    enc
                }
                None => self
                    .encoder
                    .insert(ReedSolomonEncoder::new(data, recovery, shard)?),
            };
            for pkt in &self.packets[group_start..group_start + data] {
                encoder.add_original_shard(&pkt[VideoHeader::LEN..])?;
            }
            let result = encoder.encode()?;
            for (i, rec) in result.recovery_iter().enumerate() {
                header.shard_index = (data + i) as u16;
                if self.len == self.packets.len() {
                    self.packets.push(Vec::new());
                }
                let pkt = &mut self.packets[self.len];
                self.len += 1;
                pkt.resize(VideoHeader::LEN + shard, 0);
                header.write(&mut pkt[..VideoHeader::LEN]);
                pkt[VideoHeader::LEN..].copy_from_slice(rec);
            }
        }
        Ok(&self.packets[..self.len])
    }

    fn next_packet(&mut self) -> &mut Vec<u8> {
        if self.len == self.packets.len() {
            self.packets.push(Vec::new());
        }
        let pkt = &mut self.packets[self.len];
        self.len += 1;
        pkt.resize(VideoHeader::LEN + self.cfg.shard_size, 0);
        pkt
    }
}

/// Picks FEC redundancy from reported packet loss: 10 % floor, 30 % cap,
/// roughly twice the loss rate on top of the floor, smoothed so a single
/// bad report doesn't swing it.
#[derive(Clone, Copy, Debug)]
pub struct AdaptiveRedundancy {
    pub min: f32,
    pub max: f32,
    smoothed_loss: f32,
}

impl Default for AdaptiveRedundancy {
    fn default() -> Self {
        Self {
            min: 0.10,
            max: 0.30,
            smoothed_loss: 0.0,
        }
    }
}

impl AdaptiveRedundancy {
    const ALPHA: f32 = 0.3;

    /// Feeds one loss report (0.0–1.0) and returns the new redundancy.
    pub fn update(&mut self, loss_ratio: f32) -> f32 {
        let loss = loss_ratio.clamp(0.0, 1.0);
        self.smoothed_loss += Self::ALPHA * (loss - self.smoothed_loss);
        self.redundancy()
    }

    pub fn redundancy(&self) -> f32 {
        (self.min + 2.0 * self.smoothed_loss).clamp(self.min, self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fernsicht_proto::Packet;

    fn meta() -> FrameMeta {
        FrameMeta {
            session_id: 1,
            frame_id: 10,
            keyframe: false,
            capture_us: 1_000,
            capture_ready_delta_us: 100,
            encoded_delta_us: 3_000,
        }
    }

    #[test]
    fn rejects_bad_config() {
        let odd = FecConfig {
            shard_size: 1001,
            ..FecConfig::default()
        };
        assert!(Packetizer::new(odd).is_err());
    }

    #[test]
    fn small_frame_single_group() {
        let cfg = FecConfig {
            shard_size: 100,
            max_data_per_group: 64,
            redundancy: 0.2,
        };
        let mut p = Packetizer::new(cfg).unwrap();
        let frame: Vec<u8> = (0..250u32).map(|i| i as u8).collect();
        let packets = p.packetize(&meta(), &frame).unwrap();
        // 3 data shards, ceil(3 * 0.2) = 1 recovery shard
        assert_eq!(packets.len(), 4);
        for (i, pkt) in packets.iter().enumerate() {
            let Packet::Video(h, payload) = Packet::decode(pkt).unwrap() else {
                panic!("not video");
            };
            assert_eq!(h.shard_index as usize, i);
            assert_eq!((h.data_shards, h.recovery_shards), (3, 1));
            assert_eq!(h.frame_len, 250);
            assert_eq!(payload.len(), 100);
        }
        // last data shard is zero-padded
        let Packet::Video(_, last) = Packet::decode(&packets[2]).unwrap() else {
            unreachable!()
        };
        assert_eq!(&last[..50], &frame[200..]);
        assert!(last[50..].iter().all(|&b| b == 0));
    }

    #[test]
    fn large_frame_splits_into_groups() {
        let cfg = FecConfig {
            shard_size: 64,
            max_data_per_group: 10,
            redundancy: 0.3,
        };
        let mut p = Packetizer::new(cfg).unwrap();
        let frame = vec![7u8; 64 * 25 + 1]; // 26 data shards → groups of 10, 10, 6
        let packets = p.packetize(&meta(), &frame).unwrap();
        let mut groups = Vec::new();
        for pkt in packets {
            let Packet::Video(h, _) = Packet::decode(pkt).unwrap() else {
                panic!()
            };
            assert_eq!(h.group_count, 3);
            if h.shard_index == 0 {
                groups.push((h.group_offset, h.data_shards, h.recovery_shards));
            }
        }
        assert_eq!(groups, vec![(0, 10, 3), (640, 10, 3), (1280, 6, 2)]);
        assert_eq!(packets.len(), 10 + 3 + 10 + 3 + 6 + 2);
    }

    #[test]
    fn no_fec_when_redundancy_zero() {
        let cfg = FecConfig {
            shard_size: 100,
            max_data_per_group: 64,
            redundancy: 0.0,
        };
        let mut p = Packetizer::new(cfg).unwrap();
        assert_eq!(p.packetize(&meta(), &[1; 150]).unwrap().len(), 2);
    }

    #[test]
    fn buffers_are_reused() {
        let mut p = Packetizer::new(FecConfig::default()).unwrap();
        let frame = vec![1u8; 50_000];
        p.packetize(&meta(), &frame).unwrap();
        let ptrs: Vec<*const u8> = p.packets.iter().map(|v| v.as_ptr()).collect();
        p.packetize(&meta(), &frame).unwrap();
        let again: Vec<*const u8> = p.packets.iter().map(|v| v.as_ptr()).collect();
        assert_eq!(ptrs, again);
    }

    #[test]
    fn adaptive_redundancy_bounds() {
        let mut a = AdaptiveRedundancy::default();
        assert!((a.redundancy() - 0.10).abs() < 1e-6);
        for _ in 0..50 {
            a.update(0.5);
        }
        assert!((a.redundancy() - 0.30).abs() < 1e-6);
        for _ in 0..50 {
            a.update(0.0);
        }
        assert!(a.redundancy() < 0.11);
        let mut b = AdaptiveRedundancy::default();
        for _ in 0..50 {
            b.update(0.05);
        }
        assert!((b.redundancy() - 0.20).abs() < 0.01);
    }
}
