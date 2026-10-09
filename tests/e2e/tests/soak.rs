//! Long-running soak test: a minute of mixed impairments. Run nightly or
//! by hand: `cargo test -p fernsicht-e2e --release --test soak -- --ignored`.

use std::time::Duration;

use fernsicht_e2e::{Impairment, Outcome, Scenario};

#[test]
#[ignore = "takes a minute; run in the nightly CI job"]
fn one_minute_of_a_bad_network() {
    let bad = Impairment {
        loss: 0.02,
        duplicate: 0.01,
        reorder: 0.05,
        delay: Duration::from_millis(2),
        jitter: Duration::from_millis(4),
        blackout: Some((Duration::from_secs(30), Duration::from_millis(300))),
    };
    let sc = Scenario {
        width: 1920,
        height: 1080,
        bitrate_kbps: 20_000,
        duration: Duration::from_secs(60),
        down: bad,
        up: Impairment::loss(0.02),
        ..Scenario::default()
    };
    let o = sc.run();
    let s = &o.summary;
    assert_eq!(s.decode_errors, 0, "{s:?}");
    assert!(
        s.frames_presented >= Outcome::expected_frames(&sc) * 8 / 10,
        "{s:?}"
    );
    // Only the blackout may cost frames; random loss must be repaired.
    assert!(o.network_frame_drops() <= 30, "{s:?}");
    assert!(s.receiver.packets_recovered > 1_000, "{s:?}");
    assert!(s.total.p95 < 50_000, "{:?}", s.total);
}
