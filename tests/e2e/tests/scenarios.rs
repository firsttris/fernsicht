//! End-to-end scenarios: real host agent and client through an impaired link.
//!
//! Thresholds are deliberately loose on timing (CI machines are noisy and
//! debug builds are slow) and strict on correctness: no corrupt frames ever,
//! and no lost frames whenever FEC is supposed to cover the loss.

use std::sync::atomic::Ordering;
use std::time::Duration;

use fernsicht_core::latency::Stage;
use fernsicht_e2e::{Impairment, Outcome, Scenario};
use fernsicht_host_agent::HostStats;

fn assert_clean(o: &Outcome) {
    let s = &o.summary;
    assert!(s.session_id.is_some(), "no session: {s:?}");
    assert_eq!(
        s.decode_errors, 0,
        "corrupt frame reached the decoder: {s:?}"
    );
    // The host may shed a frame when the test machine is busy, but not
    // routinely.
    let sent = HostStats::get(&o.host.frames_sent).max(1);
    assert!(
        o.host_overflows() * 20 <= sent,
        "host dropped {} of {sent} frames",
        o.host_overflows()
    );
}

#[test]
fn clean_link_1080p60() {
    let sc = Scenario {
        width: 1920,
        height: 1080,
        bitrate_kbps: 20_000,
        ..Scenario::default()
    };
    let o = sc.run();
    let s = &o.summary;
    assert_clean(&o);
    assert_eq!(o.network_frame_drops(), 0, "{s:?}");
    assert_eq!(s.receiver.packets_lost, 0, "{s:?}");
    // Everything the host sent arrived (the last frame may be in flight).
    let sent = HostStats::get(&o.host.frames_sent);
    let completed = u64::from(s.receiver.frames_completed);
    assert!(
        completed + 2 >= sent && completed <= sent,
        "{completed} of {sent}"
    );
    assert!(
        s.frames_presented >= Outcome::expected_frames(&sc) * 8 / 10,
        "{s:?}"
    );
    assert!(s.total.p95 < 100_000, "glass-to-glass p95 {:?}", s.total);
}

#[test]
fn one_percent_loss_both_ways_is_invisible() {
    let sc = Scenario {
        down: Impairment::loss(0.01),
        up: Impairment::loss(0.01),
        ..Scenario::default()
    };
    let o = sc.run();
    let s = &o.summary;
    assert_clean(&o);
    assert!(o.link.down.dropped.load(Ordering::Relaxed) > 0);
    assert!(s.receiver.packets_recovered > 0, "{s:?}");
    assert_eq!(o.network_frame_drops(), 0, "{s:?}");
    assert!(
        s.frames_presented >= Outcome::expected_frames(&sc) * 8 / 10,
        "{s:?}"
    );
}

#[test]
fn heavy_loss_raises_fec_and_keeps_streaming() {
    let sc = Scenario {
        down: Impairment::loss(0.05),
        duration: Duration::from_secs(4),
        ..Scenario::default()
    };
    let o = sc.run();
    let s = &o.summary;
    assert_clean(&o);
    // The host reacted to the feedback: redundancy went above the 10 % floor.
    let permille = o.link.max_redundancy_permille.load(Ordering::Relaxed);
    assert!(permille >= 150, "redundancy stayed at {permille} ‰");
    assert!(s.receiver.packets_recovered > 50, "{s:?}");
    assert!(
        s.frames_presented >= Outcome::expected_frames(&sc) * 7 / 10,
        "{s:?}"
    );
}

#[test]
fn reordering_and_duplicates_are_harmless() {
    let sc = Scenario {
        down: Impairment {
            reorder: 0.2,
            duplicate: 0.1,
            ..Impairment::none()
        },
        up: Impairment {
            duplicate: 0.2,
            ..Impairment::none()
        },
        ..Scenario::default()
    };
    let o = sc.run();
    let s = &o.summary;
    assert_clean(&o);
    assert!(o.link.down.reordered.load(Ordering::Relaxed) > 0);
    assert!(o.link.down.duplicated.load(Ordering::Relaxed) > 0);
    assert_eq!(o.network_frame_drops(), 0, "{s:?}");
    assert!(
        s.frames_presented >= Outcome::expected_frames(&sc) * 8 / 10,
        "{s:?}"
    );
}

#[test]
fn network_delay_is_measured_accurately() {
    // 8 ms each way. Symmetric, so clock sync is exact and the network
    // stage must show the delay; RTT shows twice the delay.
    let delay = Impairment {
        delay: Duration::from_millis(8),
        ..Impairment::none()
    };
    let sc = Scenario {
        down: delay,
        up: delay,
        ..Scenario::default()
    };
    let o = sc.run();
    let s = &o.summary;
    assert_clean(&o);
    let rtt = s.rtt_us.expect("no clock sync");
    assert!((16_000..30_000).contains(&rtt), "rtt {rtt} µs");
    let net = s.stage(Stage::Network);
    assert!(
        (7_000..25_000).contains(&net.avg),
        "network stage {net:?} should reflect the 8 ms link delay"
    );
}

#[test]
fn jitter_does_not_lose_frames() {
    let jitter = Impairment {
        delay: Duration::from_millis(1),
        jitter: Duration::from_millis(6),
        ..Impairment::none()
    };
    let sc = Scenario {
        down: jitter,
        up: jitter,
        ..Scenario::default()
    };
    let o = sc.run();
    let s = &o.summary;
    assert_clean(&o);
    assert!(
        s.frames_presented >= Outcome::expected_frames(&sc) * 7 / 10,
        "{s:?}"
    );
}

#[test]
fn blackout_recovers_with_a_keyframe() {
    let sc = Scenario {
        down: Impairment {
            blackout: Some((Duration::from_millis(1_000), Duration::from_millis(400))),
            ..Impairment::none()
        },
        duration: Duration::from_secs(3),
        ..Scenario::default()
    };
    let o = sc.run();
    let s = &o.summary;
    assert_clean(&o);
    assert!(
        s.receiver.frames_dropped >= 10,
        "blackout lost no frames? {s:?}"
    );
    // The client asked for a keyframe and the host sent one.
    let keyframes = o.link.keyframes.load(Ordering::Relaxed);
    assert!(keyframes >= 2, "host sent {keyframes} keyframes: {s:?}");
    assert!(s.keyframes_decoded >= 2, "{s:?}");
    // And the stream came back: well over the frames before the blackout.
    assert!(s.frames_presented >= 90, "{s:?}");
}

#[test]
fn high_frame_rate_144() {
    let sc = Scenario {
        fps: 144,
        bitrate_kbps: 30_000,
        duration: Duration::from_secs(2),
        ..Scenario::default()
    };
    let o = sc.run();
    let s = &o.summary;
    assert_clean(&o);
    assert_eq!(o.network_frame_drops(), 0, "{s:?}");
    assert!(
        s.frames_presented >= Outcome::expected_frames(&sc) * 7 / 10,
        "{s:?}"
    );
}

#[test]
fn low_bitrate_small_frames() {
    let sc = Scenario {
        width: 320,
        height: 240,
        fps: 30,
        bitrate_kbps: 300,
        duration: Duration::from_secs(2),
        ..Scenario::default()
    };
    let o = sc.run();
    assert_clean(&o);
    assert!(o.summary.frames_presented >= 45, "{:?}", o.summary);
}
