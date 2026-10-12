//! Video transport over raw UDP.
//!
//! - [`fec`]: splits an encoded frame into Reed-Solomon protected shards
//! - [`reassembly`]: rebuilds frames on the receiver, latest frame wins
//! - [`pacer`]: spreads a frame's packets over part of the frame interval,
//!   at the rate the path delivers
//! - [`resend`]: the host's last frames, to answer NACKs
//! - [`clock_sync`]: NTP-style host↔client clock offset estimation
//! - [`keyframe`]: the reference chain, asking for a keyframe at once,
//!   answering each loss once
//! - [`rate`]: bitrate adaptation from the smoothed loss, queue delay and
//!   ECN marks
//! - [`delay`]: how long frames wait in queues on the way
//! - [`delivery`]: how fast the path delivers, the rate to pace at
//! - [`loss`]: deterministic packet-loss injection for testing
//! - [`socket`]: UDP sockets with buffers sized for video bursts

pub mod clock_sync;
pub mod delay;
pub mod delivery;
pub mod fec;
pub mod keyframe;
pub mod loss;
pub mod pacer;
pub mod rate;
pub mod reassembly;
pub mod resend;
pub mod socket;

pub use clock_sync::ClockSync;
pub use delay::QueueDelay;
pub use delivery::DeliveryRate;
pub use fec::{FecConfig, FrameMeta, LossEstimator, Packetizer};
pub use loss::LossSim;
pub use pacer::Pacer;
pub use rate::{RateController, Signals};
pub use reassembly::{CompletedFrame, Reassembler, ReceiverStats};
pub use resend::ResendStore;

/// Compares wrapping frame ids: `true` if `a` is newer than `b`.
#[inline]
pub fn frame_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::frame_newer;

    #[test]
    fn wrapping_frame_order() {
        assert!(frame_newer(2, 1));
        assert!(!frame_newer(1, 2));
        assert!(!frame_newer(5, 5));
        assert!(frame_newer(0, u32::MAX));
    }
}
