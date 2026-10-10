//! Video transport over raw UDP.
//!
//! - [`fec`]: splits an encoded frame into Reed-Solomon protected shards
//! - [`reassembly`]: rebuilds frames on the receiver, latest frame wins
//! - [`pacer`]: spreads a frame's packets over part of the frame interval
//! - [`clock_sync`]: NTP-style host↔client clock offset estimation
//! - [`loss`]: deterministic packet-loss injection for testing
//! - [`socket`]: UDP sockets with buffers sized for video bursts

pub mod clock_sync;
pub mod fec;
pub mod loss;
pub mod pacer;
pub mod rate;
pub mod reassembly;
pub mod socket;

pub use clock_sync::ClockSync;
pub use fec::{FecConfig, FrameMeta, LossEstimator, Packetizer};
pub use loss::LossSim;
pub use pacer::Pacer;
pub use rate::RateController;
pub use reassembly::{CompletedFrame, Reassembler, ReceiverStats};

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
