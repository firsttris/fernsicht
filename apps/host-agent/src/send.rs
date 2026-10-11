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
    // Datagrams that cross the path whole (a VPN's MTU is lower than a
    // LAN's); the client takes the shard size from each packet.
    let shard_size = match link {
        Link::Udp(l) => {
            let datagram = socket::datagram_for(socket::path_mtu(l.peer), l.peer.is_ipv6());
            let shard = fernsicht_net::fec::shard_size_for(datagram);
            if datagram < MAX_DATAGRAM {
                log::info!(
                    "session {:08x}: the path to {} carries {datagram}-byte datagrams; shards of {shard} bytes",
                    params.session_id,
                    l.peer
                );
            }
            shard
        }
        Link::Web(_) => FecConfig::default().shard_size,
    };
    let mut packetizer = Packetizer::new(FecConfig {
        shard_size,
        ..FecConfig::default()
    })?;
    // Chunks of sealed datagrams, handed to the kernel at once.
    let mut scratch = Vec::with_capacity(socket::MAX_SEGMENTS_BYTES + MAX_DATAGRAM);
    // A frame leaves before the next one is encoded.
    let budget = Duration::from_micros(clock::frame_interval_us(u32::from(params.fps)));
    let mut window_start = Instant::now();
    let (mut frames, mut bytes, mut packets) = (0u32, 0u64, 0u64);
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
        let _ = free_enc.send(enc);
        pacer.pace(pkts, budget, |chunk| {
            packets += chunk.len() as u64;
            bytes += chunk.iter().map(|p| p.len() as u64).sum::<u64>();
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
        frames += 1;
        HostStats::bump(&shared.stats.frames_sent);

        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_secs(5) {
            let secs = elapsed.as_secs_f64();
            log::info!(
                "session {:08x}: {:.1} fps, {:.1} Mbit/s, {:.0} pkt/s, FEC sized for {:.1} % loss",
                params.session_id,
                f64::from(frames) / secs,
                bytes as f64 * 8.0 / secs / 1e6,
                packets as f64 / secs,
                packetizer.config().loss * 100.0
            );
            window_start = Instant::now();
            (frames, bytes, packets) = (0, 0, 0);
        }
    }
    Ok(())
}
