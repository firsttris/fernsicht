//! Congestion before loss (D3), and asking again for what FEC cannot
//! repair (C2): real host agent and client through a bottleneck and
//! through bursts of loss.

use std::sync::atomic::Ordering;
use std::time::Duration;

use fernsicht_e2e::{Impairment, Outcome, Scenario};
use fernsicht_host_agent::HostStats;

/// 10 Mbit/s of video into a narrower link whose queue never overflows:
/// only the queue's delay (or its marks) can tell.
fn bottleneck(rate_kbps: u32, mark_ce_above: Option<Duration>) -> Outcome {
    Scenario {
        bitrate_kbps: 10_000,
        duration: Duration::from_secs(4),
        down: Impairment {
            rate_kbps,
            mark_ce_above,
            ..Impairment::none()
        },
        ..Scenario::default()
    }
    .run()
}

#[test]
fn a_queue_lowers_the_bitrate_before_anything_is_lost() {
    let o = bottleneck(6_000, None);
    let s = &o.summary;
    assert!(s.session_id.is_some(), "{s:?}");
    assert_eq!(o.link.down.dropped.load(Ordering::Relaxed), 0);
    let cuts = HostStats::get(&o.host.bitrate_cuts);
    let kbps = HostStats::get(&o.host.bitrate_kbps);
    assert!(cuts >= 1, "the bitrate stayed: {s:?}");
    // It went below what the link carries, so the queue drained.
    assert!(
        kbps > 0 && kbps < 6_000,
        "ended at {kbps} kbit/s after {cuts} cuts"
    );
    assert_eq!(s.decode_errors, 0, "{s:?}");
}

#[test]
fn congestion_marks_lower_the_bitrate_without_loss() {
    let o = bottleneck(9_000, Some(Duration::from_millis(2)));
    let s = &o.summary;
    assert_eq!(o.link.down.dropped.load(Ordering::Relaxed), 0);
    assert!(
        o.link.down.ce_marked.load(Ordering::Relaxed) > 0,
        "nothing marked"
    );
    // The client read the marks from the IP header and reported them.
    assert!(
        HostStats::get(&o.host.packets_ce) > 0,
        "no CE reported: {s:?}"
    );
    assert!(HostStats::get(&o.host.bitrate_cuts) >= 1, "{s:?}");
}

#[test]
fn shards_fec_cannot_repair_are_sent_again() {
    // Every 200 ms eight datagrams in a row: about half a 720p frame,
    // far beyond its recovery shards.
    let sc = Scenario {
        down: Impairment {
            burst: Some((Duration::from_millis(200), 8)),
            ..Impairment::none()
        },
        ..Scenario::default()
    };
    let o = sc.run();
    let s = &o.summary;
    assert_eq!(s.decode_errors, 0, "{s:?}");
    let bursts = o.link.down.dropped.load(Ordering::Relaxed) / 8;
    assert!(bursts >= 10, "{bursts} bursts");
    // FEC grows with the loss and repairs part of each burst; the rest
    // comes again.
    let resent = HostStats::get(&o.host.packets_resent);
    assert!(resent > 0, "nothing sent again: {s:?}");
    assert!(s.receiver.packets_retransmitted > 0, "{s:?}");
    // Without NACK each burst costs a frame (and a keyframe); with it, a
    // busy machine may miss one or two.
    assert!(
        o.network_frame_drops() * 4 <= bursts,
        "{} frames lost to {bursts} bursts: {s:?}",
        o.network_frame_drops()
    );
    assert!(
        s.frames_presented >= Outcome::expected_frames(&sc) * 8 / 10,
        "{s:?}"
    );
}
