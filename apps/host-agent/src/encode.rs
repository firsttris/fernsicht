//! The encode thread: frames from the slot to the send queue, a new
//! encoder for a new size, a new bitrate on the running one.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use fernsicht_capture::Frame;
use fernsicht_codec::{CodecError, EncodedFrame, Encoder};
use fernsicht_core::Slot;

use crate::backends::fit;
use crate::health::RepeatedError;
use crate::shared::{HostStats, Shared};

/// Makes an encoder for another stream size (at a bitrate, kbit/s).
pub(crate) type Rebuild =
    Box<dyn FnMut(u32, (u16, u16)) -> anyhow::Result<Box<dyn Encoder>> + Send>;

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_loop(
    mut encoder: Box<dyn Encoder>,
    mut rebuild: Rebuild,
    mut kbps: u32,
    // The stream's size now, and the box it is fitted into.
    (mut size, bounds): ((u16, u16), (u16, u16)),
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
    let (mut failed_kbps, mut failed_size) = (0, (0, 0));
    while let Some(frame) = in_slot.take() {
        if !shared.running.load(Ordering::Acquire) {
            break;
        }
        // Another monitor (switched to) has its own size: the stream takes
        // its shape instead of squeezing it into the old one. The new
        // encoder starts with a keyframe that carries the new size.
        let wanted = fit((frame.width, frame.height), bounds);
        if wanted != size && wanted != failed_size {
            match rebuild(kbps, wanted) {
                Ok(e) => {
                    log::info!("stream now {}×{}", wanted.0, wanted.1);
                    encoder = e;
                    size = wanted;
                }
                Err(e) => {
                    log::warn!("stream size {}×{}: {e:#}", wanted.0, wanted.1);
                    failed_size = wanted;
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
