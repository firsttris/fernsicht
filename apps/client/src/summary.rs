//! What a session reports when it ends.

use fernsicht_core::latency::{Stage, Summary};
use fernsicht_net::ReceiverStats;
use fernsicht_proto::Codec;

/// Result of a client run.
#[derive(Clone, Debug, Default)]
pub struct RunSummary {
    /// Session id from the host's `HelloAck`, if one arrived.
    pub session_id: Option<u32>,
    pub codec: Option<Codec>,
    /// What decoded it, e.g. "Vulkan Video" (the last decoder made).
    pub decoder: Option<&'static str>,
    pub receiver: ReceiverStats,
    pub frames_presented: u64,
    pub keyframes_decoded: u64,
    /// Decoded but not shown because a newer frame was already waiting.
    pub frames_skipped: u64,
    /// Dropped before decoding because the decoder fell behind.
    pub frames_overflowed: u64,
    /// Frames discarded because the reference chain was broken (no keyframe
    /// yet, or a frame before them was lost).
    pub frames_awaiting_keyframe: u64,
    /// Corrupt frames rejected by the decoder.
    pub decode_errors: u64,
    /// Estimated host clock − client clock at the end of the run.
    pub clock_offset_us: i64,
    pub rtt_us: Option<u64>,
    pub total: Summary,
    pub stages: Vec<(Stage, Summary)>,
    /// Pointer positions received, and pointer images completed.
    pub cursor_positions: u64,
    pub cursor_shapes: u64,
    pub audio: AudioSummary,
}

/// How the sound went.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioSummary {
    /// 5 ms frames played from what arrived.
    pub played: u64,
    /// Frames lost and bridged by the decoder.
    pub concealed: u64,
    /// Frames dropped to keep the delay down.
    pub dropped: u64,
    /// From the host's speakers to ours, average (µs).
    pub delay_us: u64,
}

impl RunSummary {
    pub fn stage(&self, stage: Stage) -> Summary {
        self.stages
            .iter()
            .find(|(s, _)| *s == stage)
            .map(|(_, sum)| *sum)
            .unwrap_or_default()
    }
}
