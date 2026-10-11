//! The decode/present thread: decoders, the reference chain and the
//! presenter.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use fernsicht_codec::synthetic::SyntheticDecoder;
use fernsicht_codec::{CodecError, DecodedFrame, Decoder};
use fernsicht_core::latency::{FrameTimings, LatencyStats};
use fernsicht_core::now_us;
use fernsicht_net::keyframe::RefChain;
use fernsicht_proto::Codec;
use fernsicht_render::overlay::StreamInfo;
use fernsicht_render::{Presenter, overlay};
use serde::{Deserialize, Serialize};

use crate::config::{DecoderChoice, OverlayOutput};
use crate::cursor::CursorTracker;
use crate::input::InputHandle;
use crate::monitors::{MonitorControl, MonitorList};
use crate::network::{OVERLAY_INTERVAL, ReceivedFrame};

/// What the present thread needs to create a hardware decoder.
pub(crate) struct Hw {
    #[cfg_attr(not(feature = "vaapi"), allow(dead_code))]
    pub(crate) render_node: String,
    pub(crate) decoder: DecoderChoice,
}

impl Hw {
    /// Whether one of the allowed hardware decoders reports `codec`.
    #[allow(unused_variables)]
    pub(crate) fn decodes(&self, codec: Codec) -> bool {
        #[cfg(feature = "vaapi")]
        if matches!(self.decoder, DecoderChoice::Auto | DecoderChoice::Vaapi)
            && fernsicht_codec::vaapi::decodes(&self.render_node, codec)
        {
            return true;
        }
        #[cfg(feature = "nvidia")]
        if matches!(self.decoder, DecoderChoice::Auto | DecoderChoice::Nvdec)
            && fernsicht_codec::nvidia::decodes(0, codec)
        {
            return true;
        }
        false
    }
}

fn make_decoder(codec: Codec, hw: &Hw) -> Result<Box<dyn Decoder>, CodecError> {
    match codec {
        Codec::Synthetic => Ok(Box::new(SyntheticDecoder::default())),
        Codec::H264 | Codec::Hevc | Codec::Av1 => make_hw_decoder(codec, hw),
    }
}

#[allow(unused_variables)]
fn make_hw_decoder(codec: Codec, hw: &Hw) -> Result<Box<dyn Decoder>, CodecError> {
    let label = overlay::codec_label(codec);
    let mut errors = Vec::new();
    if matches!(hw.decoder, DecoderChoice::Auto | DecoderChoice::Vaapi) {
        #[cfg(feature = "vaapi")]
        if !fernsicht_codec::vaapi::decodes(&hw.render_node, codec) {
            errors.push(format!("VAAPI: {} does not decode {label}", hw.render_node));
        } else {
            match fernsicht_codec::vaapi::VaapiDecoder::for_codec(&hw.render_node, codec) {
                Ok(d) => {
                    log::info!("decoding {label} with VAAPI ({})", hw.render_node);
                    return Ok(Box::new(d));
                }
                Err(e) => errors.push(format!("VAAPI: {e}")),
            }
        }
        #[cfg(not(feature = "vaapi"))]
        errors.push("VAAPI: not in this build (cargo feature \"vaapi\")".to_string());
    }
    if matches!(hw.decoder, DecoderChoice::Auto | DecoderChoice::Nvdec) {
        #[cfg(feature = "nvidia")]
        if !fernsicht_codec::nvidia::decodes(0, codec) {
            errors.push(format!("NVDEC: this GPU does not decode {label}"));
        } else {
            match fernsicht_codec::nvidia::NvdecDecoder::for_codec(0, codec) {
                Ok(d) => {
                    log::info!("decoding {label} with NVDEC");
                    return Ok(Box::new(d));
                }
                Err(e) => errors.push(format!("NVDEC: {e}")),
            }
        }
        #[cfg(not(feature = "nvidia"))]
        errors.push("NVDEC: not in this build (cargo feature \"nvidia\")".to_string());
    }
    Err(CodecError::Backend(format!(
        "no {label} decoder: {}",
        errors.join("; ")
    )))
}

/// What the client tells the app once a second (`--app`, one JSON line on
/// stdout): the overlay's numbers and the host's monitors once it listed
/// them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppStats {
    #[serde(flatten)]
    pub overlay: overlay::Stats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitors: Option<MonitorList>,
}

#[derive(Default)]
pub(crate) struct PresentResult {
    pub(crate) stats: LatencyStats,
    pub(crate) presented: u64,
    pub(crate) skipped: u64,
    pub(crate) keyframes: u64,
    pub(crate) awaiting_keyframe: u64,
    pub(crate) decode_errors: u64,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn present_loop(
    queue: &Receiver<ReceivedFrame>,
    info: &Mutex<StreamInfo>,
    free_tx: &Sender<ReceivedFrame>,
    need_keyframe: &AtomicBool,
    print_overlay: OverlayOutput,
    hw: &Hw,
    mut record: Option<&mut dyn std::io::Write>,
    presenter: &mut dyn Presenter,
    cursor: &Mutex<CursorTracker>,
    monitors: &MonitorControl,
    input: Option<&InputHandle>,
) -> PresentResult {
    // Created on the first frame, from the codec the host announced.
    let mut decoder: Option<(Codec, Box<dyn Decoder>)> = None;
    let mut decoded = DecodedFrame::default();
    let mut chain = RefChain::default();
    let mut r = PresentResult::default();
    let mut last_overlay = Instant::now();

    while let Ok(rf) = queue.recv() {
        if decoder.as_ref().is_none_or(|(codec, _)| *codec != rf.codec) {
            decoder = make_decoder(rf.codec, hw)
                .inspect_err(|e| {
                    log::error!("no decoder for {}: {e}", overlay::codec_label(rf.codec))
                })
                .ok()
                .map(|d| (rf.codec, d));
            chain = RefChain::default();
        }
        let Some((_, decoder)) = decoder.as_mut() else {
            r.decode_errors += 1;
            let _ = free_tx.send(rf);
            continue;
        };
        if !chain.accept(rf.header.frame_id, rf.header.keyframe) {
            r.awaiting_keyframe += 1;
        } else {
            if let Some(w) = record.as_mut()
                && let Err(e) = w.write_all(&rf.data)
            {
                log::error!("recording stopped: {e}");
                record = None;
            }
            match decoder.decode(&rf.data, &mut decoded) {
                Ok(()) => {
                    let decoded_us = now_us();
                    r.keyframes += u64::from(decoded.keyframe);
                    // The host's stream takes the shape of the monitor shown;
                    // clicks map onto the picture as it is now.
                    if decoded.keyframe
                        && decoded.width > 0
                        && let Some(input) = input
                    {
                        input.set_stream((decoded.width, decoded.height));
                    }
                    // Latest frame wins at presentation: if a newer frame is
                    // already queued, showing this one would only add latency.
                    if !queue.is_empty() {
                        r.skipped += 1;
                    } else {
                        r.presented +=
                            u64::from(show(presenter, decoder.as_mut(), &decoded, cursor));
                        r.stats.record(&timings(&rf, decoded_us, now_us()));
                    }
                }
                Err(e) => {
                    chain.break_chain();
                    if matches!(e, CodecError::NeedKeyframe) {
                        r.awaiting_keyframe += 1;
                    } else {
                        r.decode_errors += 1;
                        log::debug!("decode: {e}");
                    }
                }
            }
        }
        need_keyframe.store(chain.needs_keyframe(), Ordering::Relaxed);
        let _ = free_tx.send(rf);

        if last_overlay.elapsed() >= OVERLAY_INTERVAL {
            show_overlay(&r.stats, info, presenter, print_overlay, monitors);
            last_overlay = Instant::now();
        }
    }
    r
}

/// Shows a decoded frame (the decoder's GPU picture where the presenter
/// takes one) with the pointer; whether that worked.
fn show(
    presenter: &mut dyn Presenter,
    decoder: &mut dyn Decoder,
    decoded: &DecodedFrame,
    cursor: &Mutex<CursorTracker>,
) -> bool {
    // The last picture's memory is free for the decoder once the GPU has
    // shown it (long done by now).
    presenter.settle();
    let picture = match presenter.wants() {
        Some(kind) => decoder.picture(kind).unwrap_or_else(|e| {
            log::debug!("picture: {e}");
            None
        }),
        None => None,
    };
    let pointer = cursor.lock().ok().and_then(|t| t.overlay());
    presenter
        .present(decoded, picture.as_ref(), pointer.as_ref())
        .inspect_err(|e| log::warn!("present: {e}"))
        .is_ok()
}

/// A frame's way from capture to screen on this machine's clock: the
/// host's times moved by the clock offset when the frame completed.
pub(crate) fn timings(rf: &ReceivedFrame, decoded_us: u64, presented_us: u64) -> FrameTimings {
    let h = &rf.header;
    let captured = h.capture_us as i64 - rf.offset_us;
    FrameTimings {
        captured,
        capture_ready: captured + i64::from(h.capture_ready_delta_us),
        encoded: captured + i64::from(h.encoded_delta_us),
        received: rf.completed_us as i64,
        decoded: decoded_us as i64,
        presented: presented_us as i64,
    }
}

/// The overlay on the picture, and on stdout as text or JSON if asked.
fn show_overlay(
    stats: &LatencyStats,
    info: &Mutex<StreamInfo>,
    presenter: &mut dyn Presenter,
    print: OverlayOutput,
    monitors: &MonitorControl,
) {
    let info = *info.lock().unwrap();
    let lines = overlay::lines(stats, &info);
    presenter.overlay(&lines);
    match print {
        OverlayOutput::Off => {}
        OverlayOutput::Text => {
            for line in &lines {
                println!("{line}");
            }
            println!();
        }
        OverlayOutput::Json => {
            let line = AppStats {
                overlay: overlay::stats(stats, &info),
                monitors: monitors.list(),
            };
            if let Ok(json) = serde_json::to_string(&line) {
                println!("{json}");
            }
        }
    }
}
