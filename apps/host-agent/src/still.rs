//! A screen that does not change is not encoded at every refresh: the
//! encoder, the network and the client's decoder rest, and the next change
//! still goes out at once.

use std::time::Duration;

/// After the last change, frames keep coming this long: the encoder
/// refines a still picture over a few frames.
pub const SETTLE: Duration = Duration::from_millis(500);

/// While nothing changes, a frame this often keeps the stream fresh.
pub const REFRESH: Duration = Duration::from_secs(1);

/// Whether to encode a captured frame: when it changed, when the client
/// waits for a keyframe, while the picture settles after a change, and
/// now and then while it stays still.
pub fn should_encode(
    changed: bool,
    keyframe_wanted: bool,
    since_change: Duration,
    since_encoded: Duration,
) -> bool {
    changed || keyframe_wanted || since_change < SETTLE || since_encoded >= REFRESH
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_change_goes_out_at_once() {
        assert!(should_encode(true, false, 10_000 * MS, Duration::ZERO));
    }

    #[test]
    fn a_still_picture_settles_then_refreshes_slowly() {
        // Just after the last change: the encoder refines.
        assert!(should_encode(false, false, 100 * MS, 16 * MS));
        // Settled: only now and then.
        assert!(!should_encode(false, false, 600 * MS, 16 * MS));
        assert!(!should_encode(false, false, 5_000 * MS, 999 * MS));
        assert!(should_encode(false, false, 5_000 * MS, 1_000 * MS));
    }

    #[test]
    fn a_keyframe_request_is_answered_while_still() {
        assert!(should_encode(false, true, 5_000 * MS, 16 * MS));
    }
}
