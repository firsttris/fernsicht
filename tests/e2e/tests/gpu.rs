//! Real H.264 and HEVC end to end: host agent with a hardware encoder, client with
//! the matching hardware decoder, through the impaired link. Needs a GPU:
//! `FERNSICHT_GPU=amd|intel` uses VAAPI (feature `vaapi`), `nvidia` uses
//! NVENC/NVDEC (feature `nvidia`); skips otherwise. Stage latencies go into
//! the CI job summary.
#![cfg(any(feature = "vaapi", feature = "nvidia"))]

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use fernsicht_client::{ClientConfig, CodecChoice, DecoderChoice, RunSummary, run};
use fernsicht_core::latency::Stage;
use fernsicht_e2e::{Host, ImpairedLink, Impairment, exclusive};
use fernsicht_host_agent::{EncoderKind, HostConfig, HostStats};
use fernsicht_proto::Codec;

/// The hardware path under test.
struct Gpu {
    name: &'static str,
    encoder: EncoderKind,
    decoder: DecoderChoice,
    render_node: String,
    /// What "auto" must end up with when this GPU is host and client.
    best: Codec,
}

fn gpu() -> Option<Gpu> {
    let node =
        std::env::var("FERNSICHT_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    match std::env::var("FERNSICHT_GPU").as_deref() {
        #[cfg(feature = "vaapi")]
        Ok("amd" | "intel") => Some(Gpu {
            name: "VAAPI",
            encoder: EncoderKind::Vaapi {
                render_node: node.clone(),
            },
            decoder: DecoderChoice::Vaapi,
            render_node: node,
            // RX 7800 XT (VCN 4): AV1 both ways.
            best: Codec::Av1,
        }),
        #[cfg(feature = "nvidia")]
        Ok("nvidia") => Some(Gpu {
            name: "NVENC/NVDEC",
            encoder: EncoderKind::Nvenc { gpu: 0 },
            decoder: DecoderChoice::Nvdec,
            render_node: node,
            // GTX 1080 (Pascal): HEVC, no AV1.
            best: Codec::Hevc,
        }),
        other => {
            eprintln!("skipped: no hardware path for FERNSICHT_GPU={other:?} in this build");
            None
        }
    }
}

fn stream(gpu: &Gpu, codec: CodecChoice, down: Impairment) -> (RunSummary, u64) {
    let _serial = exclusive();
    let host = Host::start(HostConfig {
        encoder: gpu.encoder.clone(),
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
            render_node: gpu.render_node.clone(),
            decoder: gpu.decoder,
            codec,
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
    let Some(gpu) = gpu() else { return };
    let (s, overflows) = stream(&gpu, CodecChoice::H264, Impairment::none());
    report(
        &format!("Echtes H.264 ({}) 1080p60, Loopback", gpu.name),
        &s,
    );
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
    let Some(gpu) = gpu() else { return };
    let (s, overflows) = stream(&gpu, CodecChoice::H264, Impairment::loss(0.01));
    report(
        &format!("Echtes H.264 ({}) 1080p60, 1 % Paketverlust", gpu.name),
        &s,
    );
    assert_eq!(s.decode_errors, 0, "{s:?}");
    assert!(s.receiver.packets_recovered > 0, "{s:?}");
    assert!(
        u64::from(s.receiver.frames_dropped) <= overflows,
        "FEC must hide 1 % loss: {s:?}"
    );
    assert!(s.frames_presented >= 4 * 60 * 8 / 10, "{s:?}");
}

#[test]
fn auto_takes_the_best_codec_over_loopback() {
    let Some(gpu) = gpu() else { return };
    let (s, overflows) = stream(&gpu, CodecChoice::Auto, Impairment::none());
    report(
        &format!(
            "Automatisch: {:?} ({}) 1080p60, Loopback",
            gpu.best, gpu.name
        ),
        &s,
    );
    assert_eq!(s.codec, Some(gpu.best), "{s:?}");
    assert_eq!(s.decode_errors, 0, "{s:?}");
    assert_eq!(s.receiver.packets_lost, 0, "{s:?}");
    assert!(u64::from(s.receiver.frames_dropped) <= overflows, "{s:?}");
    assert!(s.frames_presented >= 4 * 60 * 8 / 10, "{s:?}");
    assert!(s.total.p95 < 50_000, "{:?}", s.total);
}

#[test]
fn hevc_with_one_percent_loss() {
    let Some(gpu) = gpu() else { return };
    let (s, overflows) = stream(&gpu, CodecChoice::Hevc, Impairment::loss(0.01));
    report(
        &format!("HEVC ({}) 1080p60, 1 % Paketverlust", gpu.name),
        &s,
    );
    assert_eq!(s.codec, Some(Codec::Hevc), "{s:?}");
    assert_eq!(s.decode_errors, 0, "{s:?}");
    assert!(s.receiver.packets_recovered > 0, "{s:?}");
    assert!(
        u64::from(s.receiver.frames_dropped) <= overflows,
        "FEC must hide 1 % loss: {s:?}"
    );
    assert!(s.frames_presented >= 4 * 60 * 8 / 10, "{s:?}");
}

#[test]
fn av1_with_one_percent_loss() {
    let Some(gpu) = gpu() else { return };
    if gpu.best != Codec::Av1 {
        eprintln!("skipped: {} has no AV1", gpu.name);
        return;
    }
    let (s, overflows) = stream(&gpu, CodecChoice::Av1, Impairment::loss(0.01));
    report(&format!("AV1 ({}) 1080p60, 1 % Paketverlust", gpu.name), &s);
    assert_eq!(s.codec, Some(Codec::Av1), "{s:?}");
    assert_eq!(s.decode_errors, 0, "{s:?}");
    assert!(
        u64::from(s.receiver.frames_dropped) <= overflows,
        "FEC must hide 1 % loss: {s:?}"
    );
    assert!(s.frames_presented >= 4 * 60 * 8 / 10, "{s:?}");
}
