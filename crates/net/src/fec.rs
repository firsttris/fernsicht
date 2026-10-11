//! Frame → FEC-protected datagrams.

use fernsicht_proto::{DEFAULT_SHARD_SIZE, MAX_FRAME_LEN, VideoHeader};
use reed_solomon_simd::ReedSolomonEncoder;
use thiserror::Error;

/// Probability that a group stays unrecoverable that recovery sizing aims
/// for. With one group per frame (more only past 512 shards) and 60 fps,
/// that is about two lost frames an hour at the design loss rate, at most:
/// whole recovery shards round it down.
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
    /// Upper bound for data shards per FEC group. Larger groups survive
    /// longer bursts for the same overhead: any `recovery` losses in the
    /// group are repaired, wherever they fall.
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
            // One group for frames up to ~600 kB: P-frames and most keyframes.
            max_data_per_group: MAX_DATA_PER_GROUP,
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

/// Group `group` of `count` for a frame of `data_total` data shards: its
/// first shard and its data shards. Balanced: the sizes differ by one at
/// most, so no group is left with a few shards and little protection.
fn balanced(data_total: usize, count: usize, group: usize) -> (usize, usize) {
    let (base, extra) = (data_total / count, data_total % count);
    (
        group * base + group.min(extra),
        base + usize::from(group < extra),
    )
}

/// Writes group `g`'s fields into `header`.
fn set_group(header: &mut VideoHeader, g: usize, group: &Group, shard: usize) {
    header.group_index = g as u16;
    header.group_offset = (group.first_shard * shard) as u32;
    header.data_shards = group.data as u16;
    header.recovery_shards = group.recovery as u16;
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
    /// The current frame's groups.
    groups: Vec<Group>,
    /// The packets on their way into sending order.
    sending: Vec<Vec<u8>>,
}

/// One FEC group of a frame: where its data and recovery packets are
/// before they are put in sending order.
#[derive(Clone, Copy, Debug)]
struct Group {
    first_shard: usize,
    data: usize,
    /// Index of its first recovery packet.
    recovery_at: usize,
    recovery: usize,
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
            groups: Vec::new(),
            sending: Vec::new(),
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

        self.groups.clear();
        let mut recovery_at = data_total;
        for g in 0..group_count {
            let (first_shard, data) = balanced(data_total, group_count, g);
            let recovery = self.cfg.recovery_for(data);
            self.groups.push(Group {
                first_shard,
                data,
                recovery_at,
                recovery,
            });
            recovery_at += recovery;
        }

        // Data first, all groups, then their parity: without loss a frame is
        // complete with its last data shard, not only after the parity of
        // the groups before it. Each data packet sits at its shard number.
        for g in 0..group_count {
            let group = self.groups[g];
            let offset = group.first_shard * shard;
            set_group(&mut header, g, &group, shard);
            for i in 0..group.data {
                let start = offset + i * shard;
                let end = (start + shard).min(frame.len());
                header.shard_index = i as u16;
                let pkt = self.next_packet();
                header.write(&mut pkt[..VideoHeader::LEN]);
                let payload = &mut pkt[VideoHeader::LEN..];
                payload[..end - start].copy_from_slice(&frame[start..end]);
                payload[end - start..].fill(0);
            }
        }
        for g in 0..group_count {
            let group = self.groups[g];
            if group.recovery == 0 {
                continue;
            }
            set_group(&mut header, g, &group, shard);
            let encoder = match &mut self.encoder {
                Some(enc) => {
                    enc.reset(group.data, group.recovery, shard)?;
                    enc
                }
                None => {
                    self.encoder
                        .insert(ReedSolomonEncoder::new(group.data, group.recovery, shard)?)
                }
            };
            let data = group.first_shard..group.first_shard + group.data;
            for pkt in &self.packets[data] {
                encoder.add_original_shard(&pkt[VideoHeader::LEN..])?;
            }
            let result = encoder.encode()?;
            for (i, rec) in result.recovery_iter().enumerate() {
                header.shard_index = (group.data + i) as u16;
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
        if group_count > 1 {
            self.interleave();
        }
        Ok(&self.packets[..self.len])
    }

    /// Puts the packets in sending order: the groups' data shards in turn,
    /// then their recovery shards in turn, so that a burst of losses
    /// spreads over the groups instead of breaking one. Moves the buffers,
    /// copies nothing.
    fn interleave(&mut self) {
        let groups = &self.groups;
        let most_data = groups.iter().map(|g| g.data).max().unwrap_or(0);
        let most_recovery = groups.iter().map(|g| g.recovery).max().unwrap_or(0);
        let data = (0..most_data).flat_map(|i| {
            groups
                .iter()
                .filter(move |g| i < g.data)
                .map(move |g| g.first_shard + i)
        });
        let recovery = (0..most_recovery).flat_map(|i| {
            groups
                .iter()
                .filter(move |g| i < g.recovery)
                .map(move |g| g.recovery_at + i)
        });
        let mut sending = std::mem::take(&mut self.sending);
        sending.clear();
        for i in data.chain(recovery) {
            sending.push(std::mem::take(&mut self.packets[i]));
        }
        self.packets.splice(..self.len, sending.drain(..));
        self.sending = sending;
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
        // 26 data shards → three balanced groups of 9, 9 and 8 (not 10,
        // 10 and a weak 6).
        let frame = vec![7u8; 64 * 25 + 1];
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
        assert_eq!(groups, vec![(0, 9, 3), (576, 9, 3), (1152, 8, 3)]);
        assert_eq!(packets.len(), 26 + 3 * 3);
    }

    #[test]
    fn groups_are_balanced_and_cover_the_frame() {
        for total in 1..200 {
            for count in 1..=total.min(9) {
                let groups: Vec<_> = (0..count).map(|g| balanced(total, count, g)).collect();
                let mut next = 0;
                for &(first, data) in &groups {
                    assert_eq!(first, next, "{total} in {count}");
                    next += data;
                }
                assert_eq!(next, total);
                let sizes = groups.iter().map(|g| g.1);
                assert!(sizes.clone().max().unwrap() - sizes.min().unwrap() <= 1);
            }
        }
    }

    #[test]
    fn a_frame_up_to_512_shards_is_one_group() {
        let mut p = Packetizer::new(FecConfig::default()).unwrap();
        let frame = vec![1u8; 500 * DEFAULT_SHARD_SIZE];
        let packets = p.packetize(&meta(), &frame).unwrap();
        let Packet::Video(h, _) = Packet::decode(&packets[0]).unwrap() else {
            panic!()
        };
        assert_eq!((h.group_count, h.data_shards), (1, 500));
    }

    #[test]
    fn a_burst_spreads_over_the_groups() {
        let cfg = FecConfig {
            shard_size: 64,
            max_data_per_group: 10,
            redundancy: 0.3,
            loss: 0.0,
            ..FecConfig::default()
        };
        let mut p = Packetizer::new(cfg).unwrap();
        let frame: Vec<u8> = (0..64 * 26).map(|i| i as u8).collect();
        let packets = p.packetize(&meta(), &frame).unwrap().to_vec();
        // Nine packets in a row lost: three of each group, as many as its
        // recovery shards repair. In frame order they would break a group.
        let mut r = crate::Reassembler::new();
        let done = packets
            .iter()
            .enumerate()
            .filter(|(i, _)| !(5..14).contains(i))
            .find_map(|(_, pkt)| {
                let Ok(Packet::Video(h, payload)) = Packet::decode(pkt) else {
                    panic!()
                };
                r.push(&h, payload, 0).map(|f| f.data.to_vec())
            });
        assert_eq!(done.as_deref(), Some(&frame[..]));
    }

    #[test]
    fn data_goes_before_parity_and_the_groups_take_turns() {
        let cfg = FecConfig {
            shard_size: 64,
            max_data_per_group: 10,
            redundancy: 0.3,
            loss: 0.0,
            ..FecConfig::default()
        };
        let mut p = Packetizer::new(cfg).unwrap();
        let packets = p.packetize(&meta(), &vec![7u8; 64 * 25 + 1]).unwrap();
        let order: Vec<(u16, bool)> = packets
            .iter()
            .map(|pkt| {
                let Packet::Video(h, _) = Packet::decode(pkt).unwrap() else {
                    panic!()
                };
                (h.group_index, h.shard_index >= h.data_shards)
            })
            .collect();
        // All 26 data shards, then the parity.
        assert!(order[..26].iter().all(|&(_, parity)| !parity));
        assert!(order[26..].iter().all(|&(_, parity)| parity));
        // Never two packets of one group in a row.
        assert!(order.windows(2).all(|w| w[0].0 != w[1].0), "{order:?}");
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
