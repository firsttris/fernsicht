//! Client: a network thread (receive, reassemble, clock sync, feedback)
//! and a decode/present thread.
//!
//! ```text
//! [network] --fifo(4)--> [decode] --latest wins--> [present]
//! ```
//!
//! Compressed frames reference earlier ones, so every received frame is
//! decoded in order; only *decoded* frames are skipped when presentation
//! falls behind. Any gap in the frame ids breaks the reference chain: the
//! decoder then waits for a keyframe and the client asks the host for one.
//!
//! The threads live in `network`, `present` and `audio`; `config`, `hosts`,
//! `input`, `monitors` and `summary` hold what an app hands in and gets back.

mod audio;
mod config;
mod cursor;
pub mod discover;
mod hosts;
mod input;
mod monitors;
mod network;
mod present;
mod summary;
#[cfg(test)]
mod tests;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use crossbeam_channel::bounded;
use fernsicht_core::latency::Stage;
use fernsicht_core::thread::{make_current_hot, spawn_hot};
pub use fernsicht_input::{InputEvent, buttons};
use fernsicht_render::overlay::StreamInfo;
use fernsicht_render::{HeadlessPresenter, Presenter};
pub use fernsicht_secure::{Identity, Peer, PublicKey, Trusted};

use crate::audio::{AudioPacket, audio_loop};
use crate::cursor::CursorTracker;
use crate::network::{Pipe, ReceivedFrame, network_loop};
use crate::present::{Hw, present_loop};

pub use config::{
    AudioOutput, ClientConfig, ClientSecurity, CodecChoice, DecoderChoice, OverlayOutput,
    offered_codecs,
};
pub use hosts::{
    DEFAULT_PORT, PAIR_TIMEOUT, default_state_dir, find_host, is_address, pair, printable,
    reject_text, remember_address, with_default_port,
};
pub use input::{
    InputHandle, MAX_CHORD, chord_events, pad_events_while_focused, parse_keys_command,
    window_to_stream,
};
pub use monitors::{MonitorControl, MonitorList, MonitorRequest, parse_monitor_command};
pub use present::RefChain;
pub use summary::{AudioSummary, RunSummary};

/// Completed frames waiting for the decoder.
const DECODE_QUEUE: usize = 4;

/// Frame buffers: the queue, one being decoded, one being filled.
const BUFFERS: usize = DECODE_QUEUE + 2;

/// Creates the presenter on the present thread (Vulkan objects stay on the
/// thread that uses them).
pub type PresenterFactory = Box<dyn FnOnce() -> Result<Box<dyn Presenter>, String> + Send>;

/// Connects to `cfg.host` and streams until `stop`, the configured duration,
/// or the host going away. Frames are decoded but not shown.
pub fn run(cfg: ClientConfig, stop: Arc<AtomicBool>) -> anyhow::Result<RunSummary> {
    run_with(
        cfg,
        stop,
        Box::new(|| Ok(Box::new(HeadlessPresenter::default()) as Box<dyn Presenter>)),
    )
}

/// Like [`run`], showing frames with the presenter `make_presenter`
/// creates. If that fails, the client runs headless and says why.
pub fn run_with(
    cfg: ClientConfig,
    stop: Arc<AtomicBool>,
    make_presenter: PresenterFactory,
) -> anyhow::Result<RunSummary> {
    let socket = fernsicht_net::socket::connect_udp(&cfg.host)
        .with_context(|| format!("connect {}", cfg.host))?;
    socket.set_read_timeout(Some(Duration::from_millis(2)))?;
    // Input, feedback and pings: small and urgent (WMM voice on Wi-Fi).
    if let Err(e) =
        fernsicht_net::socket::set_dscp(&socket, fernsicht_net::socket::Dscp::Interactive)
    {
        log::debug!("no DSCP marking: {e}");
    }
    // Bursts of video datagrams come in one receive.
    if !fernsicht_net::socket::enable_gro(&socket) {
        log::debug!("no UDP GRO: one datagram per receive");
    }

    let (decode_tx, decode_rx) = bounded::<ReceivedFrame>(DECODE_QUEUE);
    let need_keyframe = Arc::new(AtomicBool::new(true));
    let info = Arc::new(Mutex::new(StreamInfo::default()));
    let cursor = Arc::new(Mutex::new(CursorTracker::default()));
    let (free_tx, free_rx) = bounded(BUFFERS);
    for _ in 0..BUFFERS {
        free_tx.send(ReceivedFrame::empty()).unwrap();
    }

    let mut record = match &cfg.record {
        Some(path) => Some(std::io::BufWriter::new(
            std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?,
        )),
        None => None,
    };
    let presenter = {
        let (info, free_tx, need_keyframe) = (info.clone(), free_tx.clone(), need_keyframe.clone());
        let cursor = cursor.clone();
        let monitors = cfg.monitors.clone();
        let input = cfg.input.clone();
        let print = cfg.print_overlay;
        let hw = Hw {
            render_node: cfg.render_node.clone(),
            decoder: cfg.decoder,
        };
        spawn_hot("present", move || {
            let mut presenter = make_presenter().unwrap_or_else(|e| {
                log::error!("no video window ({e}); running headless");
                Box::new(HeadlessPresenter::default())
            });
            let r = present_loop(
                &decode_rx,
                &info,
                &free_tx,
                &need_keyframe,
                print,
                &hw,
                record.as_mut().map(|w| w as &mut dyn std::io::Write),
                presenter.as_mut(),
                &cursor,
                &monitors,
                input.as_deref(),
            );
            if let Some(Err(e)) = record.as_mut().map(std::io::Write::flush) {
                log::error!("recording: {e}");
            }
            r
        })?
    };

    // Receives the picture and sends the input.
    make_current_hot("network");
    // Sound plays on its own thread, paced by the sound card.
    let (audio_tx, audio_rx) = bounded::<AudioPacket>(64);
    let audio = match cfg.audio.clone() {
        AudioOutput::Off => None,
        out => {
            let muted = cfg.muted.clone();
            Some(spawn_hot("audio", move || {
                audio_loop(&audio_rx, &out, &muted)
            })?)
        }
    };
    let pipe = Pipe {
        audio_tx: audio.as_ref().map(|_| &audio_tx),
        decode_tx: &decode_tx,
        free_rx: &free_rx,
        free_tx: &free_tx,
        need_keyframe: &need_keyframe,
        cursor: &cursor,
    };
    let net = network_loop(&cfg, &socket, &stop, &pipe, &info);
    // Closing the queue ends the present thread once it has drained.
    drop(decode_tx);
    drop(audio_tx);
    let present = presenter.join().expect("present thread panicked");
    let audio = audio
        .map(|t| t.join().expect("audio thread panicked"))
        .unwrap_or_default();
    let net = net?;
    let (cursor_positions, cursor_shapes) = cursor
        .lock()
        .map(|t| (t.positions, t.shapes))
        .unwrap_or_default();

    Ok(RunSummary {
        session_id: net.session.map(|(id, _)| id),
        codec: net.session.map(|(_, codec)| codec),
        receiver: net.receiver,
        frames_presented: present.presented,
        keyframes_decoded: present.keyframes,
        frames_skipped: present.skipped,
        frames_overflowed: net.overflowed,
        frames_awaiting_keyframe: present.awaiting_keyframe,
        decode_errors: present.decode_errors,
        clock_offset_us: net.clock_offset_us,
        rtt_us: net.rtt_us,
        total: present.stats.total(),
        stages: Stage::ALL
            .iter()
            .map(|s| (*s, present.stats.stage(*s)))
            .collect(),
        cursor_positions,
        cursor_shapes,
        audio,
    })
}
