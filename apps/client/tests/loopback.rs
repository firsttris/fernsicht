//! End-to-end: host agent and client over UDP on localhost.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fernsicht_client::{ClientConfig, run};
use fernsicht_host_agent::{HostAgent, HostConfig};

fn stream(loss: f64, secs: u64) -> fernsicht_client::RunSummary {
    let agent = HostAgent::bind(HostConfig {
        bind: "127.0.0.1:0".into(),
        ..HostConfig::default()
    })
    .unwrap();
    let addr = agent.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let host = {
        let stop = stop.clone();
        std::thread::spawn(move || agent.run(stop))
    };
    let summary = run(
        ClientConfig {
            host: addr.to_string(),
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_kbps: 10_000,
            loss,
            duration: Some(Duration::from_secs(secs)),
            print_overlay: false,
        },
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    host.join().unwrap().unwrap();
    summary
}

#[test]
fn streams_over_loopback() {
    let s = stream(0.0, 2);
    // ~2 s at 60 fps, minus session setup; generous for loaded CI machines.
    assert!(s.frames_presented >= 60, "{s:?}");
    assert_eq!(s.decode_errors, 0, "{s:?}");
    assert_eq!(s.receiver.frames_dropped, 0, "{s:?}");
    assert!(s.total.samples > 0);
}

#[test]
fn one_percent_loss_is_invisible() {
    let s = stream(0.01, 3);
    assert!(s.frames_presented >= 90, "{s:?}");
    assert!(s.receiver.packets_recovered > 0, "{s:?}");
    assert_eq!(s.receiver.frames_dropped, 0, "{s:?}");
    assert_eq!(s.decode_errors, 0, "{s:?}");
}
