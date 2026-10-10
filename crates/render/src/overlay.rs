//! Latency overlay content, as shown top right in the client session view.
//!
//! ```text
//! Glass-to-Glass  14 ms
//! Capture 1 ms · Encode 4 ms · Netz 3 ms · Decode 2 ms · Anzeige 4 ms
//! Codec H.264 · VAAPI   Bildrate 60 fps   Bitrate 20 Mbit/s   Verlust (FEC) 0,4 % → 0
//! ```

use std::fmt::Write;

use fernsicht_core::latency::{LatencyStats, Stage};
use fernsicht_proto::Codec;

/// Everything the overlay shows besides the per-stage latencies.
#[derive(Clone, Copy, Debug, Default)]
pub struct StreamInfo {
    pub codec: Codec,
    pub fps: f32,
    pub bitrate_bps: u64,
    /// Packet loss on the wire, before FEC (0.0–1.0).
    pub loss_before_fec: f32,
    /// Frames lost after FEC (0.0–1.0).
    pub loss_after_fec: f32,
    pub rtt_us: Option<u64>,
}

pub fn codec_label(codec: Codec) -> &'static str {
    match codec {
        Codec::Synthetic => "Synthetisch",
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
    }
}

/// Milliseconds with one decimal, German style ("3,4 ms").
pub fn ms(us: u32) -> String {
    let tenths = (us + 50) / 100;
    format!("{},{} ms", tenths / 10, tenths % 10)
}

/// Percentage with one decimal; exact zero prints as "0".
pub fn percent(ratio: f32) -> String {
    if ratio <= 0.0 {
        return "0".into();
    }
    let tenths = (ratio * 1000.0).round() as u32;
    format!("{},{} %", tenths / 10, tenths % 10)
}

pub fn mbit(bps: u64) -> String {
    format!("{} Mbit/s", (bps + 500_000) / 1_000_000)
}

/// Overlay as text lines (the terminal client prints them; the Vulkan
/// client will draw the same values).
pub fn lines(stats: &LatencyStats, info: &StreamInfo) -> Vec<String> {
    let total = stats.total();
    let mut out = Vec::with_capacity(3);
    out.push(format!(
        "Glass-to-Glass {}  (p95 {}, max {})",
        ms(total.avg),
        ms(total.p95),
        ms(total.max)
    ));
    let mut stages = String::new();
    for (i, stage) in Stage::ALL.iter().enumerate() {
        if i > 0 {
            stages.push_str(" · ");
        }
        let _ = write!(stages, "{} {}", stage.label(), ms(stats.stage(*stage).avg));
    }
    out.push(stages);
    let mut meta = format!(
        "Codec {} · Bildrate {:.0} fps · Bitrate {} · Verlust (FEC) {} → {}",
        codec_label(info.codec),
        info.fps,
        mbit(info.bitrate_bps),
        percent(info.loss_before_fec),
        percent(info.loss_after_fec),
    );
    if let Some(rtt) = info.rtt_us {
        let _ = write!(meta, " · RTT {}", ms(rtt as u32));
    }
    out.push(meta);
    out
}

/// The same values as one JSON object, for the desktop app (the shape of
/// its `SessionStats`, plus glass-to-glass and RTT).
pub fn json(stats: &LatencyStats, info: &StreamInfo) -> String {
    let total = stats.total();
    let mut stages = String::new();
    for (i, stage) in Stage::ALL.iter().enumerate() {
        let key = match stage {
            Stage::Capture => "capture",
            Stage::Encode => "encode",
            Stage::Network => "network",
            Stage::Decode => "decode",
            Stage::Present => "present",
        };
        let sep = if i > 0 { "," } else { "" };
        let _ = write!(stages, "{sep}\"{key}\":{}", stats.stage(*stage).avg);
    }
    let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
    format!(
        "{{\"glassToGlassUs\":{{\"avg\":{},\"p95\":{},\"max\":{}}},\"stagesUs\":{{{stages}}},\
         \"codec\":\"{}\",\"fps\":{},\"bitrateBps\":{},\"lossBeforeFec\":{},\
         \"lossAfterFec\":{},\"rttUs\":{}}}",
        total.avg,
        total.p95,
        total.max,
        codec_label(info.codec),
        finite(info.fps),
        info.bitrate_bps,
        finite(info.loss_before_fec),
        finite(info.loss_after_fec),
        info.rtt_us.map_or("null".into(), |r| r.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fernsicht_core::latency::FrameTimings;

    #[test]
    fn number_formatting() {
        assert_eq!(ms(14_000), "14,0 ms");
        assert_eq!(ms(3_449), "3,4 ms");
        assert_eq!(ms(3_450), "3,5 ms");
        assert_eq!(percent(0.0), "0");
        assert_eq!(percent(0.004), "0,4 %");
        assert_eq!(mbit(38_200_000), "38 Mbit/s");
    }

    #[test]
    fn overlay_lines() {
        let mut stats = LatencyStats::default();
        stats.record(&FrameTimings {
            captured: 0,
            capture_ready: 1_000,
            encoded: 5_000,
            received: 8_000,
            decoded: 10_000,
            presented: 14_000,
        });
        let info = StreamInfo {
            codec: Codec::Av1,
            fps: 120.0,
            bitrate_bps: 38_000_000,
            loss_before_fec: 0.004,
            loss_after_fec: 0.0,
            rtt_us: None,
        };
        let l = lines(&stats, &info);
        assert!(l[0].starts_with("Glass-to-Glass 14,0 ms"));
        assert_eq!(
            l[1],
            "Capture 1,0 ms · Encode 4,0 ms · Netz 3,0 ms · Decode 2,0 ms · Anzeige 4,0 ms"
        );
        assert_eq!(
            l[2],
            "Codec AV1 · Bildrate 120 fps · Bitrate 38 Mbit/s · Verlust (FEC) 0,4 % → 0"
        );

        let j: serde_json::Value = serde_json::from_str(&json(&stats, &info)).unwrap();
        assert_eq!(j["glassToGlassUs"]["avg"], 14_000);
        assert_eq!(j["stagesUs"]["capture"], 1_000);
        assert_eq!(j["stagesUs"]["network"], 3_000);
        assert_eq!(j["stagesUs"]["present"], 4_000);
        assert_eq!(j["codec"], "AV1");
        assert_eq!(j["fps"], 120.0);
        assert_eq!(j["bitrateBps"], 38_000_000);
        assert!((j["lossBeforeFec"].as_f64().unwrap() - 0.004).abs() < 1e-6);
        assert_eq!(j["rttUs"], serde_json::Value::Null);
        let with_rtt = StreamInfo {
            rtt_us: Some(1_250),
            fps: f32::NAN,
            ..info
        };
        let j: serde_json::Value = serde_json::from_str(&json(&stats, &with_rtt)).unwrap();
        assert_eq!(j["rttUs"], 1_250);
        assert_eq!(j["fps"], 0.0, "never NaN, which JSON cannot carry");
    }
}
