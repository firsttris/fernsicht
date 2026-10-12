//! The send thread: encoded frames into FEC-protected, paced datagrams,
//! and the shards the client asks for again (NACK).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use fernsicht_codec::EncodedFrame;
use fernsicht_core::clock;
use fernsicht_net::{FecConfig, FrameMeta, LossSim, Pacer, Packetizer, ResendStore, pacer, socket};
use fernsicht_proto::{MAX_DATAGRAM, Nack};

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
    let mut resend = ResendStore::default();
    // The configured rate: what pacing never exceeds.
    let cap = pacer.rate();

    loop {
        let enc = crossbeam_channel::select! {
            recv(queue) -> enc => match enc {
                Ok(enc) => enc,
                Err(_) => break,
            },
            recv(shared.nacks.1) -> nack => {
                if let Ok(nack) = nack {
                    answer(shared, link, &resend, &nack, &mut scratch, &mut errors)?;
                }
                continue;
            }
        };
        if !shared.running.load(Ordering::Acquire) {
            break;
        }
        packetizer.set_loss(f32::from_bits(shared.fec_loss.load(Ordering::Relaxed)));
        let bitrate = match shared.target_kbps.load(Ordering::Relaxed) {
            0 => params.bitrate_kbps,
            kbps => kbps,
        };
        let delivery = shared.delivery_kbps.load(Ordering::Relaxed);
        pacer.set_rate(pacer::rate_for_delivery(cap, delivery, bitrate));
        let frame_id = enc.frame_id;
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
            transient_ok(link.send_chunk(kept, &mut scratch), &mut errors)
        })?;
        // After it left: keeping it costs the frame no time.
        resend.keep(frame_id, pkts);
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

/// Sends again what `nack` asks for, if it is still kept: unpaced, a few
/// packets, sealed afresh.
fn answer(
    shared: &Shared,
    link: &Link,
    resend: &ResendStore,
    nack: &Nack,
    scratch: &mut Vec<u8>,
    errors: &mut RepeatedError,
) -> std::io::Result<()> {
    let counted = resend.find(nack).inspect(|_| {
        HostStats::bump(&shared.stats.packets_resent);
    });
    transient_ok(link.send_chunk(counted, scratch), errors)
}

/// The network gone for a moment is packet loss, not a reason to stop.
fn transient_ok(sent: std::io::Result<()>, errors: &mut RepeatedError) -> std::io::Result<()> {
    match sent {
        Err(e) if crate::health::send_error_is_transient(&e) => {
            if let Some(line) = errors.report(e.to_string(), Instant::now()) {
                log::warn!("send: {line}");
            }
            Ok(())
        }
        other => other,
    }
}
