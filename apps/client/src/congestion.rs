//! What the client tells the host about the path (D3, C2): each frame's
//! queue delay, and the shards FEC cannot repair, asked for again.

use std::time::Duration;

use fernsicht_proto::{MAX_NACK_SHARDS, Nack, ShardRef, VideoHeader};

/// A frame FEC cannot complete is asked for again once its packets have
/// been quiet this long: its sending is over (the pacer's chunks follow
/// each other within a fraction of it).
pub(crate) const NACK_QUIET_US: u64 = 2_000;

/// At most this many NACKs (of [`MAX_NACK_SHARDS`] shards) per frame; a
/// frame that lost more lost it to a blackout, which a keyframe heals.
pub(crate) const MAX_NACKS_PER_FRAME: usize = 4;

/// Whether a retransmission arrives before the next frame is due: the
/// round trip and the wait for the frame's last packets fit in a frame
/// interval (a LAN; further away a keyframe or the refresh heals the loss).
pub(crate) fn nack_in_time(rtt_us: Option<u64>, frame_interval: Duration) -> bool {
    rtt_us.is_some_and(|rtt| Duration::from_micros(rtt + NACK_QUIET_US) < frame_interval)
}

/// The NACKs that ask for `shards` of frame `frame_id`.
pub(crate) fn nacks(
    session_id: u32,
    frame_id: u32,
    shards: &[ShardRef],
) -> impl Iterator<Item = Nack> + '_ {
    shards
        .chunks(MAX_NACK_SHARDS)
        .take(MAX_NACKS_PER_FRAME)
        .map(move |part| Nack::new(session_id, frame_id, part))
}

/// A frame's way from the encoder to its first packet here, on two clocks
/// (their offset cancels in [`fernsicht_net::QueueDelay`]).
pub(crate) fn queue_sample(h: &VideoHeader, first_us: u64) -> i64 {
    let encoded = h.capture_us as i64 + i64::from(h.encoded_delta_us);
    first_us as i64 - encoded
}
