//! Latency overlay content, as shown top right in the client session view.
//!
//! ```text
//! Glass-to-Glass  14 ms
//! Capture 1 ms · Encode 4 ms · Netz 3 ms · Decode 2 ms · Anzeige 4 ms
//! Codec H.264 · VAAPI   Bildrate 60 fps   Bitrate 20 Mbit/s   Verlust (FEC) 0,4 % → 0
//! ```

use std::fmt::Write;

use fernsicht_core::colour::{Colour, Depth};
use fernsicht_core::latency::{LatencyStats, Stage};
use fernsicht_proto::Codec;
use serde::{Deserialize, Serialize};

/// Everything the overlay shows besides the per-stage latencies.
#[derive(Clone, Copy, Debug, Default)]
pub struct StreamInfo {
    pub codec: Codec,
    /// What decodes it here, e.g. "Vulkan Video"; `None` before the first
    /// frame and for the synthetic codec.
    pub decoder: Option<&'static str>,
    /// The stream's colour, from the last decoded frame.
    pub colour: Colour,
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

/// The codec, its depth and HDR where it has them, and what decodes it,
/// e.g. "HEVC 10 Bit HDR · Vulkan Video".
pub fn codec_and_decoder(info: &StreamInfo) -> String {
    let mut label = codec_label(info.codec).to_owned();
    if info.colour.depth == Depth::Ten {
        label.push_str(" 10 Bit");
    }
    if info.colour.is_hdr() {
        label.push_str(" HDR");
    }
    match info.decoder {
        Some(decoder) => format!("{label} · {decoder}"),
        None => label,
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
        codec_and_decoder(info),
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

/// The overlay's numbers for the desktop app, which reads them as JSON from
/// the client's output once a second.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Stats {
    pub glass_to_glass_us: Spread,
    /// Average per stage.
    pub stages_us: Stages,
    pub codec: String,
    pub fps: f32,
    pub bitrate_bps: u64,
    /// Packets lost on the way, before FEC: 0–1.
    pub loss_before_fec: f32,
    /// Frames lost despite FEC: 0–1.
    pub loss_after_fec: f32,
    /// Round trip to the host, where known.
    pub rtt_us: Option<u64>,
}

/// Average, 95th percentile and maximum, µs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Spread {
    pub avg: u32,
    pub p95: u32,
    pub max: u32,
}

/// One duration per pipeline stage, µs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Stages {
    pub capture: u32,
    pub encode: u32,
    pub network: u32,
    pub decode: u32,
    pub present: u32,
}

/// The values of [`lines`] for the desktop app.
pub fn stats(stats: &LatencyStats, info: &StreamInfo) -> Stats {
    let total = stats.total();
    let avg = |s: Stage| stats.stage(s).avg;
    // JSON has no NaN.
    let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
    Stats {
        glass_to_glass_us: Spread {
            avg: total.avg,
            p95: total.p95,
            max: total.max,
        },
        stages_us: Stages {
            capture: avg(Stage::Capture),
            encode: avg(Stage::Encode),
            network: avg(Stage::Network),
            decode: avg(Stage::Decode),
            present: avg(Stage::Present),
        },
        codec: codec_and_decoder(info),
        fps: finite(info.fps),
        bitrate_bps: info.bitrate_bps,
        loss_before_fec: finite(info.loss_before_fec),
        loss_after_fec: finite(info.loss_after_fec),
        rtt_us: info.rtt_us,
    }
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
            decoder: None,
            colour: Colour::SDR,
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

        let j = serde_json::to_value(super::stats(&stats, &info)).unwrap();
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
        let j = serde_json::to_value(super::stats(&stats, &with_rtt)).unwrap();
        assert_eq!(j["rttUs"], 1_250);
        assert_eq!(j["fps"], 0.0, "never NaN, which JSON cannot carry");
        // Once a decoder runs, it is named with the codec.
        let decoding = StreamInfo {
            decoder: Some("Vulkan Video"),
            ..info
        };
        assert_eq!(super::stats(&stats, &decoding).codec, "AV1 · Vulkan Video");
        assert!(lines(&stats, &decoding)[2].starts_with("Codec AV1 · Vulkan Video · Bildrate"));
        // A 10-bit stream, and an HDR one, say so.
        let deep = StreamInfo {
            colour: Colour::SDR.with_depth(Depth::Ten),
            ..decoding
        };
        assert_eq!(codec_and_decoder(&deep), "AV1 10 Bit · Vulkan Video");
        let hdr = StreamInfo {
            colour: Colour::HDR10,
            ..decoding
        };
        assert_eq!(codec_and_decoder(&hdr), "AV1 10 Bit HDR · Vulkan Video");
        // What the app reads back is what was written.
        let s = super::stats(&stats, &with_rtt);
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Stats>(&text).unwrap(), s);
    }
}
