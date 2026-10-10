//! Real H.264 end to end: host agent with the VAAPI encoder, client with the
//! VAAPI decoder, through the impaired link. Needs a GPU: runs when
//! `FERNSICHT_GPU=amd` (the GPU CI job on the self-hosted runner), skips
//! otherwise. Stage latencies go into the CI job summary.
#![cfg(feature = "vaapi")]

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use fernsicht_client::{ClientConfig, RunSummary, run};
use fernsicht_core::latency::Stage;
use fernsicht_e2e::{Host, ImpairedLink, Impairment, exclusive};
use fernsicht_host_agent::{EncoderKind, HostConfig, HostStats};
use fernsicht_proto::Codec;

fn render_node() -> Option<String> {
    match std::env::var("FERNSICHT_GPU").as_deref() {
        Ok("amd" | "intel") => Some(
            std::env::var("FERNSICHT_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into()),
        ),
        _ => {
            eprintln!("skipped: set FERNSICHT_GPU=amd (needs a VAAPI GPU)");
            None
        }
    }
}

fn stream(node: &str, down: Impairment) -> (RunSummary, u64) {
    let _serial = exclusive();
    let host = Host::start(HostConfig {
        encoder: EncoderKind::Vaapi {
            render_node: node.into(),
        },
        ..HostConfig::default()
    });
    let stats = host.stats();
    let link = ImpairedLink::start(host.addr(), down, Impairment::none(), 7);
    let summary = run(
        ClientConfig {
            host: link.addr().to_string(),
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            duration: Some(Duration::from_secs(4)),
            render_node: node.into(),
            ..ClientConfig::default()
        },
        Arc::new(AtomicBool::new(false)),
    )
    .expect("client failed");
    drop(link);
    host.stop();
    (summary, HostStats::get(&stats.send_overflows))
}

fn report(title: &str, s: &RunSummary) {
    let ms = |us: u32| f64::from(us) / 1000.0;
    let mut lines = vec![
        format!("### {title}"),
        "| Stufe | Ø | p95 |".into(),
        "|---|---|---|".into(),
    ];
    for stage in Stage::ALL {
        let x = s.stage(stage);
        lines.push(format!(
            "| {} | {:.2} ms | {:.2} ms |",
            stage.label(),
            ms(x.avg),
            ms(x.p95)
        ));
    }
    lines.push(format!(
        "| **Glass-to-Glass** (ohne Bildschirm) | **{:.2} ms** | {:.2} ms |",
        ms(s.total.avg),
        ms(s.total.p95)
    ));
    lines.push(format!(
        "\nFrames: {} angezeigt, {} übersprungen, {} verloren · Pakete repariert: {}\n",
        s.frames_presented,
        s.frames_skipped,
        s.receiver.frames_dropped,
        s.receiver.packets_recovered
    ));
    for l in &lines {
        println!("{l}");
    }
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY")
        && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path)
    {
        for l in &lines {
            let _ = writeln!(f, "{l}");
        }
    }
}

#[test]
fn real_h264_over_loopback() {
    let Some(node) = render_node() else { return };
    let (s, overflows) = stream(&node, Impairment::none());
    report("Echtes H.264 (VAAPI) 1080p60, Loopback", &s);
    assert_eq!(s.codec, Some(Codec::H264));
    assert_eq!(s.decode_errors, 0, "{s:?}");
    assert_eq!(s.receiver.packets_lost, 0, "{s:?}");
    assert!(u64::from(s.receiver.frames_dropped) <= overflows, "{s:?}");
    assert!(s.frames_presented >= 4 * 60 * 8 / 10, "{s:?}");
    // Generous: catches a stalled pipeline, not a slow machine.
    assert!(s.total.p95 < 50_000, "{:?}", s.total);
}

#[test]
fn real_h264_with_one_percent_loss() {
    let Some(node) = render_node() else { return };
    let (s, overflows) = stream(&node, Impairment::loss(0.01));
    report("Echtes H.264 (VAAPI) 1080p60, 1 % Paketverlust", &s);
    assert_eq!(s.decode_errors, 0, "{s:?}");
    assert!(s.receiver.packets_recovered > 0, "{s:?}");
    assert!(
        u64::from(s.receiver.frames_dropped) <= overflows,
        "FEC must hide 1 % loss: {s:?}"
    );
    assert!(s.frames_presented >= 4 * 60 * 8 / 10, "{s:?}");
}
