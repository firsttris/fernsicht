//! Frame → FEC-protected datagrams.

use fernsicht_proto::{DEFAULT_SHARD_SIZE, MAX_FRAME_LEN, VideoHeader};
use reed_solomon_simd::ReedSolomonEncoder;
use thiserror::Error;

/// Probability that a group stays unrecoverable that recovery sizing aims
/// for. At 60 fps with one to three groups per frame this is roughly one
/// lost frame per hour at the design loss rate.
pub const TARGET_GROUP_FAILURE: f64 = 1e-5;

/// Upper bound for `max_data_per_group`. With `max_redundancy ≤ 1` a group
/// then stays within the protocol's `MAX_GROUP_SHARDS`.
pub const MAX_DATA_PER_GROUP: usize = 512;

/// Small groups may always use this many recovery shards, whatever
/// `max_redundancy` says: tiny frames (a static desktop) are cheap to
/// protect, and one recovery shard alone fails too often.
pub const MIN_RECOVERY_CAP: usize = 3;

#[derive(Clone, Copy, Debug)]
pub struct FecConfig {
    /// Payload bytes per datagram. Must be even.
    pub shard_size: usize,
    /// Upper bound for data shards per FEC group. Smaller groups decode
    /// faster and isolate burst loss; larger groups protect better.
    pub max_data_per_group: usize,
    /// Minimum recovery shards as a fraction of data shards (0.0 disables
    /// FEC entirely).
    pub redundancy: f32,
    /// Upper bound for the recovery fraction.
    pub max_redundancy: f32,
    /// Packet loss rate (0–1) the recovery shards are sized for. With 0,
    /// only `redundancy` applies.
    pub loss: f32,
}

impl Default for FecConfig {
    fn default() -> Self {
        Self {
            shard_size: DEFAULT_SHARD_SIZE,
            max_data_per_group: 64,
            redundancy: 0.10,
            max_redundancy: 0.50,
            loss: LossEstimator::FLOOR,
        }
    }
}

impl FecConfig {
    /// Recovery shards for a group of `data_shards`.
    ///
    /// At least `redundancy` of the data, then enough that independent loss
    /// at rate `loss` makes the group unrecoverable with probability at most
    /// [`TARGET_GROUP_FAILURE`], capped at `max_redundancy` (but always
    /// allowing [`MIN_RECOVERY_CAP`]).
    pub fn recovery_for(&self, data_shards: usize) -> usize {
        if self.redundancy <= 0.0 || data_shards == 0 {
            return 0;
        }
        let base = ((data_shards as f32 * self.redundancy).ceil() as usize).max(1);
        let cap = ((data_shards as f32 * self.max_redundancy).ceil() as usize)
            .max(base)
            .max(MIN_RECOVERY_CAP);
        if self.loss <= 0.0 {
            return base;
        }
        let p = f64::from(self.loss.min(0.5));
        (base..=cap)
            .find(|&r| group_failure(data_shards, r, p) <= TARGET_GROUP_FAILURE)
            .unwrap_or(cap)
    }
}

/// Probability that more than `recovery` of `data + recovery` shards are
/// lost when each is lost independently with probability `p`.
pub fn group_failure(data: usize, recovery: usize, p: f64) -> f64 {
    let n = data + recovery;
    if p <= 0.0 {
        return 0.0;
    }
    // Binomial pmf, iteratively: pmf(k+1) = pmf(k) · (n−k)/(k+1) · p/(1−p).
    let mut pmf = (1.0 - p).powi(n as i32);
    let mut cdf = pmf;
    for k in 0..recovery {
        pmf *= (n - k) as f64 / (k + 1) as f64 * p / (1.0 - p);
        cdf += pmf;
    }
    (1.0 - cdf).max(0.0)
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
        if cfg.shard_size == 0 || !cfg.shard_size.is_multiple_of(2) {
            return Err(FecError::Config("shard_size must be even and non-zero"));
        }
        if cfg.max_data_per_group == 0 || cfg.max_data_per_group > MAX_DATA_PER_GROUP {
            return Err(FecError::Config("max_data_per_group must be 1..=512"));
        }
        if !(0.0..=1.0).contains(&cfg.max_redundancy) {
            return Err(FecError::Config("max_redundancy must be within 0..=1"));
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

    /// Changes the loss rate FEC is sized for (adaptive FEC).
    pub fn set_loss(&mut self, loss: f32) {
        self.cfg.loss = loss.clamp(0.0, 1.0);
    }

    /// Splits `frame` into datagrams. The returned packets stay valid until
    /// the next call.
    pub fn packetize(&mut self, meta: &FrameMeta, frame: &[u8]) -> Result<&[Vec<u8>], FecError> {
        if frame.is_empty() {
            return Err(FecError::Empty);
        }
        if frame.len() > MAX_FRAME_LEN as usize {
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

/// Turns loss reports into the loss rate FEC is sized for: smoothed so a
/// single bad report doesn't swing it, and never below [`Self::FLOOR`] so a
/// clean link still survives the occasional burst.
#[derive(Clone, Copy, Debug, Default)]
pub struct LossEstimator {
    smoothed: f32,
}

impl LossEstimator {
    /// Design for at least 1 % loss (phase 1 acceptance criterion).
    pub const FLOOR: f32 = 0.01;
    const ALPHA: f32 = 0.3;

    /// Feeds one loss report (0.0–1.0) and returns the new estimate.
    pub fn update(&mut self, loss_ratio: f32) -> f32 {
        let loss = if loss_ratio.is_nan() {
            0.0
        } else {
            loss_ratio.clamp(0.0, 1.0)
        };
        self.smoothed += Self::ALPHA * (loss - self.smoothed);
        self.estimate()
    }

    pub fn estimate(&self) -> f32 {
        self.smoothed.max(Self::FLOOR)
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
        for bad in [
            FecConfig {
                shard_size: 1001,
                ..FecConfig::default()
            },
            FecConfig {
                shard_size: 0,
                ..FecConfig::default()
            },
            FecConfig {
                max_data_per_group: 0,
                ..FecConfig::default()
            },
            FecConfig {
                max_data_per_group: MAX_DATA_PER_GROUP + 1,
                ..FecConfig::default()
            },
            FecConfig {
                max_redundancy: 1.5,
                ..FecConfig::default()
            },
        ] {
            assert!(Packetizer::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn rejects_empty_and_oversized_frames() {
        let mut p = Packetizer::new(FecConfig::default()).unwrap();
        assert!(matches!(p.packetize(&meta(), &[]), Err(FecError::Empty)));
        let huge = vec![0u8; MAX_FRAME_LEN as usize + 1];
        assert!(matches!(
            p.packetize(&meta(), &huge),
            Err(FecError::TooLarge(_))
        ));
    }

    #[test]
    fn largest_groups_fit_the_protocol() {
        let cfg = FecConfig {
            max_data_per_group: MAX_DATA_PER_GROUP,
            max_redundancy: 1.0,
            loss: 0.5,
            ..FecConfig::default()
        };
        let total = MAX_DATA_PER_GROUP + cfg.recovery_for(MAX_DATA_PER_GROUP);
        assert!(total <= fernsicht_proto::MAX_GROUP_SHARDS as usize);
    }

    #[test]
    fn small_frame_single_group() {
        let cfg = FecConfig {
            shard_size: 100,
            max_data_per_group: 64,
            redundancy: 0.2,
            loss: 0.0,
            ..FecConfig::default()
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
            loss: 0.0,
            ..FecConfig::default()
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
            loss: 0.0,
            ..FecConfig::default()
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
    fn loss_estimator_smooths_and_floors() {
        let mut e = LossEstimator::default();
        assert_eq!(e.estimate(), LossEstimator::FLOOR);
        for _ in 0..50 {
            e.update(0.2);
        }
        assert!((e.estimate() - 0.2).abs() < 1e-3);
        // One clean report only moves it part of the way.
        assert!(e.update(0.0) > 0.1);
        for _ in 0..50 {
            e.update(0.0);
        }
        assert_eq!(e.estimate(), LossEstimator::FLOOR);
        assert_eq!(e.update(f32::NAN), LossEstimator::FLOOR);
    }

    #[test]
    fn group_failure_matches_closed_forms() {
        // No recovery: failure = 1 − (1−p)^n.
        let p = 0.01;
        assert!((group_failure(10, 0, p) - (1.0 - 0.99f64.powi(10))).abs() < 1e-12);
        // One recovery shard over 2 shards: both lost = p².
        assert!((group_failure(1, 1, p) - p * p).abs() < 1e-12);
        assert_eq!(group_failure(5, 2, 0.0), 0.0);
        // More recovery never hurts.
        assert!(group_failure(16, 4, p) < group_failure(16, 3, p));
    }

    #[test]
    fn recovery_sized_for_one_percent_loss() {
        let cfg = FecConfig::default();
        for d in [1, 4, 16, 32, 64] {
            let r = cfg.recovery_for(d);
            assert!(r as f32 >= d as f32 * 0.1, "d={d} r={r}");
            assert!(
                group_failure(d, r, 0.01) <= TARGET_GROUP_FAILURE,
                "d={d} r={r}"
            );
            // Minimal: one shard less would miss the target (unless at floor).
            let floor = ((d as f32 * 0.1).ceil() as usize).max(1);
            if r > floor {
                assert!(group_failure(d, r - 1, 0.01) > TARGET_GROUP_FAILURE);
            }
        }
        // Large groups need proportionally less.
        assert!(cfg.recovery_for(64) * 16 < cfg.recovery_for(16) * 64);
    }

    #[test]
    fn recovery_is_capped() {
        let cfg = FecConfig {
            loss: 0.4,
            ..FecConfig::default()
        };
        assert_eq!(cfg.recovery_for(20), 10);
        assert_eq!(cfg.recovery_for(2), MIN_RECOVERY_CAP);
        let off = FecConfig {
            redundancy: 0.0,
            ..FecConfig::default()
        };
        assert_eq!(off.recovery_for(20), 0);
        assert_eq!(cfg.recovery_for(0), 0);
    }
}
