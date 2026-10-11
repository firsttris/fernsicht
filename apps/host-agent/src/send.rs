//! The send thread: encoded frames into FEC-protected, paced datagrams.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use fernsicht_codec::EncodedFrame;
use fernsicht_core::clock;
use fernsicht_net::{FecConfig, FrameMeta, LossSim, Pacer, Packetizer, socket};
use fernsicht_proto::MAX_DATAGRAM;

use crate::config::SessionParams;
use crate::health::RepeatedError;
use crate::link::Link;
use crate::shared::{HostStats, Shared};
use crate::throughput::{self, Sent, Window};

/// How often the host logs what a session sends.
const LOG_EVERY: Duration = Duration::from_secs(5);

#[allow(clippy::too_many_arguments)]
pub(crate) fn send_loop(
    shared: &Shared,
    queue: &Receiver<EncodedFrame>,
    free_enc: &Sender<EncodedFrame>,
    link: &Link,
    params: SessionParams,
    mut pacer: Pacer,
    mut loss: LossSim,
) -> anyhow::Result<()> {
    let shard_size = link.shard_size();
    let mut packetizer = Packetizer::new(FecConfig {
        shard_size,
        ..FecConfig::default()
    })?;
    // Chunks of sealed datagrams, handed to the kernel at once.
    let mut scratch = Vec::with_capacity(socket::MAX_SEGMENTS_BYTES + MAX_DATAGRAM);
    // A frame leaves before the next one is encoded.
    let budget = Duration::from_micros(clock::frame_interval_us(u32::from(params.fps)));
    let mut window: Window = (Instant::now(), Sent::default());
    let mut errors = RepeatedError::default();

    while let Ok(enc) = queue.recv() {
        if !shared.running.load(Ordering::Acquire) {
            break;
        }
        packetizer.set_loss(f32::from_bits(shared.fec_loss.load(Ordering::Relaxed)));
        let meta = FrameMeta {
            session_id: params.session_id,
            frame_id: enc.frame_id,
            keyframe: enc.keyframe,
            capture_us: enc.capture_us,
            capture_ready_delta_us: enc.capture_ready_us.saturating_sub(enc.capture_us) as u32,
            encoded_delta_us: enc.encoded_us.saturating_sub(enc.capture_us) as u32,
        };
        let pkts = packetizer.packetize(&meta, &enc.data)?;
        let bytes = pkts.iter().map(|p| p.len() as u64).sum();
        window.1 = throughput::count(window.1, &enc, bytes, pkts.len() as u64);
        let _ = free_enc.send(enc);
        pacer.pace(pkts, budget, |chunk| {
            let kept = chunk
                .iter()
                .map(Vec::as_slice)
                .filter(|_| !loss.drop_packet());
            match link.send_chunk(kept, &mut scratch) {
                Ok(_) => Ok(()),
                // The network gone for a moment is packet loss, not a
                // reason to stop.
                Err(e) if crate::health::send_error_is_transient(&e) => {
                    if let Some(line) = errors.report(e.to_string(), Instant::now()) {
                        log::warn!("send: {line}");
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        })?;
        HostStats::bump(&shared.stats.frames_sent);

        if let Some(r) = throughput::close_after(&mut window, Instant::now(), LOG_EVERY) {
            log::info!(
                "session {:08x}: {:.1} fps, {:.1} Mbit/s, {:.0} pkt/s, FEC sized for {:.1} % loss",
                params.session_id,
                r.fps,
                r.bits_per_sec / 1e6,
                r.packets_per_sec,
                packetizer.config().loss * 100.0
            );
        }
    }
    Ok(())
}
