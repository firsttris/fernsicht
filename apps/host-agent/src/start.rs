//! Negotiating a session's parameters and starting its pipeline.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;
use fernsicht_codec::EncodedFrame;
use fernsicht_core::thread::spawn_hot;
use fernsicht_core::{Slot, clock};
use fernsicht_input::Dedup;
use fernsicht_net::{LossEstimator, LossSim, Pacer, RateController, pacer};
use fernsicht_proto::{Codec, CodecSet, Hello, HelloAck};
use fernsicht_secure::session::Responder;

use crate::HostAgent;
use crate::audio::{audio_loop, make_audio};
use crate::backends::{default_bitrate_kbps, fit, frame_rate, make_encoder, make_source};
use crate::capture::capture_loop;
use crate::config::{CaptureKind, EncoderKind, SessionParams};
use crate::encode::{Rebuild, encode_loop};
use crate::health::{RepeatedError, StopOnExit};
use crate::input::make_input;
use crate::link::{Link, UdpLink};
use crate::monitors::MonitorSender;
use crate::pointer::CursorSender;
use crate::send::send_loop;
use crate::session::{Session, codec_label, new_session_id};
use crate::shared::{HostStats, Shared};

/// Raw frame buffers: producer, slot, consumer.
const BUFFERS: usize = 3;
/// Encoded frames waiting for the sender.
const SEND_QUEUE: usize = 2;
/// Encoded frame buffers: the queue, one being encoded, one being sent.
const ENCODED_BUFFERS: usize = SEND_QUEUE + 2;

impl HostAgent {
    /// The client's wishes capped by the host's limits. A width, height or
    /// bitrate of 0 is resolved when the session starts: the size of the
    /// captured screen, and a bitrate for that size.
    pub(crate) fn negotiate(&self, hello: &Hello) -> SessionParams {
        let pick = |want: u16, max: u16| if want == 0 { max } else { want.min(max) };
        SessionParams {
            session_id: new_session_id(),
            width: hello.width.min(self.cfg.max_width) & !1,
            height: hello.height.min(self.cfg.max_height) & !1,
            fps: pick(hello.fps, self.cfg.max_fps).max(1),
            bitrate_kbps: hello.bitrate_kbps.min(self.cfg.max_bitrate_kbps),
            codecs: hello.codecs,
        }
    }

    /// The box a session's stream is fitted into: the size the client
    /// asked for, or the host's limits when it left the size to the host.
    fn bounds(&self, p: &SessionParams) -> (u16, u16) {
        if p.width == 0 || p.height == 0 {
            (self.cfg.max_width, self.cfg.max_height)
        } else {
            (p.width, p.height)
        }
    }

    /// Fills in what the client left to the host (see [`Self::negotiate`]):
    /// the monitor's size fitted into the box, aspect ratio kept.
    fn resolve(&self, mut p: SessionParams, native: (u32, u32)) -> SessionParams {
        (p.width, p.height) = fit(native, self.bounds(&p));
        if p.bitrate_kbps == 0 {
            p.bitrate_kbps = default_bitrate_kbps(p.width, p.height, p.fps)
                .min(self.cfg.max_bitrate_kbps)
                .max(1);
        }
        p
    }

    /// Starts a session. With `secure`, the handshake is answered with the
    /// HelloAck inside and the session's packets are sealed; the answer to
    /// send is returned.
    pub(crate) fn start_session(
        &self,
        params: SessionParams,
        peer: SocketAddr,
        secure: Option<Responder>,
        web: Option<crate::web::WebSetup>,
    ) -> anyhow::Result<(Session, Option<Vec<u8>>)> {
        let loss = LossEstimator::default();
        HostStats::bump(&self.stats.sessions);
        let shared = Arc::new(Shared {
            stats: self.stats.clone(),
            running: AtomicBool::new(true),
            keyframe_requested: AtomicBool::new(true),
            last_keyframe: Mutex::new(None),
            frame: Duration::from_micros(clock::frame_interval_us(u32::from(params.fps))),
            cursor_requested: AtomicBool::new(false),
            fec_loss: AtomicU32::new(loss.estimate().to_bits()),
            target_kbps: AtomicU32::new(0),
            monitor_request: Default::default(),
            screen: Default::default(),
            screen_changes: AtomicU64::new(0),
        });
        // Source and encoder come first: if the screen or the GPU is
        // unavailable the session fails before any thread starts. The
        // source tells the size of the screen, for a client that asked for
        // the host's own resolution.
        let source = make_source(&self.cfg.capture, &params)?;
        let bounds = self.bounds(&params);
        // Before the default bitrate is worked out per frame.
        let params = SessionParams {
            fps: frame_rate(params.fps, source.refresh_hz()),
            ..params
        };
        let params = self.resolve(params, (source.width(), source.height()));
        let source = match &self.cfg.capture {
            // The test pattern is drawn at whatever size the session uses.
            CaptureKind::TestPattern | CaptureKind::TestPatternWith { .. } => {
                make_source(&self.cfg.capture, &params)?
            }
            _ => source,
        };
        let encoder = make_encoder(&self.cfg.encoder, &params)?;
        let codec = encoder.codec();
        // Rebuilt encoders (new bitrate) keep the codec the client was told.
        let params = SessionParams {
            codecs: CodecSet::EMPTY.with(codec),
            ..params
        };
        // The encoder's GPU at full clocks while the session runs.
        let gpu = match &self.cfg.encoder {
            EncoderKind::Vaapi { render_node } if self.cfg.settings.gpu_boost() => {
                let plan = crate::power::plan(Path::new("/sys"), render_node);
                Some(crate::power::GpuBoost::apply(
                    plan,
                    self.cfg.power_record.clone(),
                ))
            }
            _ => None,
        };
        shared
            .target_kbps
            .store(params.bitrate_kbps, Ordering::Relaxed);
        // A new encoder at another bitrate (for encoders that cannot change
        // it while running) or size (another monitor).
        let rebuild: Rebuild = {
            let kind = self.cfg.encoder.clone();
            Box::new(move |kbps, (width, height)| {
                make_encoder(
                    &kind,
                    &SessionParams {
                        bitrate_kbps: kbps,
                        width,
                        height,
                        ..params
                    },
                )
            })
        };
        shared.set_screen(source.screen());
        let mut input = make_input(&self.cfg.input, &shared);
        let (crypto, handshake_reply) = match secure {
            None => (None, None),
            Some(responder) => {
                let mut ack = [0u8; HelloAck::LEN];
                self.hello_ack(&params, codec).encode(&mut ack);
                let (transport, reply) = responder
                    .reply(&ack)
                    .map_err(|e| anyhow::anyhow!("handshake: {e}"))?;
                (Some(Arc::new(transport)), Some(reply))
            }
        };
        let web_out = web.as_ref().map(|_| crate::web::out_channel());
        let link = match &web_out {
            Some((tx, _)) => Link::Web(tx.clone()),
            None => Link::Udp(UdpLink {
                socket: self.socket.clone(),
                peer,
                session_id: params.session_id,
                // A plain session (tests) has no keys to prove.
                confirmed: Arc::new(AtomicBool::new(crypto.is_none())),
                gso: Arc::new(AtomicBool::new(true)),
                crypto,
            }),
        };
        let frame_slot = Arc::new(Slot::new());
        let (send_tx, send_rx) = bounded::<EncodedFrame>(SEND_QUEUE);

        let (free_frames_tx, free_frames_rx) = bounded(BUFFERS);
        for _ in 0..BUFFERS {
            free_frames_tx.send(source.alloc_frame()).unwrap();
        }
        let (free_enc_tx, free_enc_rx) = bounded(ENCODED_BUFFERS);
        for _ in 0..ENCODED_BUFFERS {
            free_enc_tx.send(EncodedFrame::default()).unwrap();
        }

        // Threads started so far; stopped and joined if a later one fails.
        let mut threads = {
            let (shared, slot) = (shared.clone(), frame_slot.clone());
            crate::health::Starting::new(move || {
                shared.running.store(false, Ordering::Release);
                slot.close();
            })
        };
        let capture = {
            let (shared, slot) = (shared.clone(), frame_slot.clone());
            let (free_rx, free_tx) = (free_frames_rx, free_frames_tx.clone());
            let cursor = CursorSender::new(
                link.clone(),
                params.session_id,
                (source.width(), source.height()),
            );
            let monitors = MonitorSender::new(link.clone());
            spawn_hot("capture", move || {
                let _guard = StopOnExit {
                    running: &shared.running,
                    thread: "capture",
                };
                capture_loop(source, &shared, &slot, &free_rx, &free_tx, cursor, monitors);
            })?
        };
        threads.add(capture);
        let encode = {
            let (shared, in_slot) = (shared.clone(), frame_slot.clone());
            let free_enc_tx = free_enc_tx.clone();
            spawn_hot("encode", move || {
                let _guard = StopOnExit {
                    running: &shared.running,
                    thread: "encode",
                };
                encode_loop(
                    encoder,
                    rebuild,
                    params.bitrate_kbps,
                    ((params.width, params.height), bounds),
                    &shared,
                    &in_slot,
                    &send_tx,
                    &free_frames_tx,
                    &free_enc_rx,
                    &free_enc_tx,
                );
            })?
        };
        threads.add(encode);
        match make_audio(self.cfg.audio) {
            Ok(None) => {}
            Ok(Some(source)) => {
                let (shared, link) = (shared.clone(), link.clone());
                let session_id = params.session_id;
                threads.add(spawn_hot("audio", move || {
                    if let Err(e) = audio_loop(source, &shared, &link, session_id) {
                        log::warn!("audio: {e}");
                    }
                })?);
            }
            Err(e) => log::warn!("no sound: {e}"),
        }
        let send = if let Some((tx, _)) = &web_out {
            let (shared, tx) = (shared.clone(), tx.clone());
            let label = codec_label(codec);
            spawn_hot("send", move || {
                let _guard = StopOnExit {
                    running: &shared.running,
                    thread: "send",
                };
                crate::web::send_loop(&shared, &send_rx, &free_enc_tx, &tx, params, label);
            })?
        } else {
            let (shared, link) = (shared.clone(), link.clone());
            let pacer = Pacer::new(self.cfg.pace_bytes_per_sec, pacer::BURST_BYTES);
            let loss = LossSim::new(self.cfg.loss, u64::from(params.session_id) | 1);
            spawn_hot("send", move || {
                let _guard = StopOnExit {
                    running: &shared.running,
                    thread: "send",
                };
                if let Err(e) =
                    send_loop(&shared, &send_rx, &free_enc_tx, &link, params, pacer, loss)
                {
                    log::error!("send: {e:#}");
                }
            })?
        };
        threads.add(send);

        // The browser's input arrives on the WebRTC thread, which injects
        // it there (the host's loop only polls every 100 ms).
        let web = match (web, web_out) {
            (Some(setup), Some((_, rx))) => {
                threads.add(crate::web::spawn(
                    setup,
                    codec,
                    shared.clone(),
                    rx,
                    input.take(),
                )?);
                true
            }
            _ => false,
        };
        let session = Session {
            params,
            codec,
            client: None,
            peer,
            web,
            link,
            handshake: None,
            last_seen: Instant::now(),
            shared,
            loss,
            rate: RateController::new(params.bitrate_kbps, Instant::now()),
            // The counter spans all sessions: this one starts from here.
            overflows_seen: HostStats::get(&self.stats.send_overflows),
            client_rtt: Duration::ZERO,
            threads: threads.done(),
            frame_slot,
            input,
            input_seen: Dedup::default(),
            input_errors: RepeatedError::default(),
            _gpu: gpu,
        };
        Ok((session, handshake_reply))
    }

    pub(crate) fn hello_ack(&self, p: &SessionParams, codec: Codec) -> HelloAck {
        HelloAck {
            session_id: p.session_id,
            width: p.width,
            height: p.height,
            fps: p.fps,
            codec,
        }
    }
}
