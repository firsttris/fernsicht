//! The encode thread: frames from the slot to the send queue, a new
//! encoder for a new size or a screen that changed its colour (HDR on or
//! off, another depth), a new bitrate on the running one.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use fernsicht_capture::Frame;
use fernsicht_codec::{CodecError, EncodedFrame, Encoder};
use fernsicht_core::Slot;
use fernsicht_core::colour::Colour;

use crate::backends::fit;
use crate::health::RepeatedError;
use crate::shared::{HostStats, Shared};

/// Makes an encoder for another stream size or screen colour (at a
/// bitrate, kbit/s).
pub(crate) type Rebuild =
    Box<dyn FnMut(u32, (u16, u16), Colour) -> anyhow::Result<Box<dyn Encoder>> + Send>;

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_loop(
    mut encoder: Box<dyn Encoder>,
    mut rebuild: Rebuild,
    mut kbps: u32,
    // The stream's size now, and the box it is fitted into.
    (mut size, bounds): ((u16, u16), (u16, u16)),
    // What the screen held when the encoder was made.
    mut screen: Colour,
    shared: &Shared,
    in_slot: &Slot<Frame>,
    send_tx: &Sender<EncodedFrame>,
    free_frames: &Sender<Frame>,
    free_enc_rx: &Receiver<EncodedFrame>,
    free_enc_tx: &Sender<EncodedFrame>,
) {
    let mut next_frame_id = 0u32;
    let mut errors = RepeatedError::default();
    // A bitrate the encoder could not take or a size it could not be
    // rebuilt for; tried again only when the wish changes, not on every
    // frame.
    let (mut failed_kbps, mut failed) = (0, None);
    while let Some(frame) = in_slot.take() {
        if !shared.running.load(Ordering::Acquire) {
            break;
        }
        // Another monitor (switched to) has its own size: the stream takes
        // its shape instead of squeezing it into the old one. A screen
        // switched to HDR, or to another depth, gets a stream in its
        // colour. The new encoder starts with a keyframe that carries both.
        let wanted = (fit((frame.width, frame.height), bounds), frame.colour);
        if wanted != (size, screen) && Some(wanted) != failed {
            match rebuild(kbps, wanted.0, wanted.1) {
                Ok(e) => {
                    let c = e.colour();
                    log::info!(
                        "stream now {}×{}, {} bit{}",
                        wanted.0.0,
                        wanted.0.1,
                        c.depth.bits(),
                        if c.is_hdr() { ", HDR" } else { "" }
                    );
                    encoder = e;
                    (size, screen) = wanted;
                }
                Err(e) => {
                    log::warn!(
                        "stream {}×{} in {:?}: {e:#}",
                        wanted.0.0,
                        wanted.0.1,
                        wanted.1
                    );
                    failed = Some(wanted);
                }
            }
        }
        let target = shared.target_kbps.load(Ordering::Relaxed);
        if target != kbps && target != failed_kbps && target > 0 {
            // On the running encoder: no new GPU objects, no gap in the
            // picture.
            match encoder.set_bitrate(target) {
                Ok(()) => kbps = target,
                Err(e) => {
                    log::warn!("bitrate {target} kbit/s: {e:#}");
                    failed_kbps = target;
                }
            }
        }
        if shared.keyframe_requested.swap(false, Ordering::Relaxed) {
            encoder.request_keyframe();
        }
        let Ok(mut out) = free_enc_rx.recv_timeout(Duration::from_millis(100)) else {
            let _ = free_frames.send(frame);
            continue;
        };
        let result = encoder.encode(&frame, &mut out);
        let _ = free_frames.send(frame);
        match result {
            Ok(()) => {
                HostStats::bump(&shared.stats.frames_encoded);
                let (frame_id, keyframe) = (next_frame_id, out.keyframe);
                if keyframe {
                    HostStats::bump(&shared.stats.keyframes_encoded);
                }
                out.frame_id = frame_id;
                (out.width, out.height) = size;
                next_frame_id = next_frame_id.wrapping_add(1);
                match send_tx.try_send(out) {
                    Ok(()) if keyframe => shared.sent_keyframe(frame_id),
                    Ok(()) => {}
                    // The sender is a whole queue behind (network or CPU
                    // overload). Later frames reference this one, so resync
                    // with a keyframe; the client sees the frame-id gap too.
                    Err(TrySendError::Full(out)) => {
                        HostStats::bump(&shared.stats.send_overflows);
                        encoder.request_keyframe();
                        let _ = free_enc_tx.send(out);
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            Err(CodecError::GpuLost) => {
                // Nothing will encode any more: the session ends instead of
                // living on without a picture.
                log::error!("encode: the GPU stopped responding; ending the session");
                break;
            }
            Err(e) => {
                if let Some(line) = errors.report(e.to_string(), Instant::now()) {
                    log::warn!("encode: {line}");
                }
                let _ = free_enc_tx.send(out);
            }
        }
    }
    // Dropping `send_tx` on return ends the send loop.
}
