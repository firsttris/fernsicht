//! Negotiating a session's parameters and starting its pipeline.

use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};
use fernsicht_capture::{Frame, FrameSource};
use fernsicht_codec::EncodedFrame;
use fernsicht_codec::Encoder;
use fernsicht_core::colour::Colour;
use fernsicht_core::thread::spawn_hot;
use fernsicht_core::{Slot, clock};
use fernsicht_input::{Dedup, InputSink};
use fernsicht_net::{LossEstimator, LossSim, Pacer, RateController, pacer};
use fernsicht_proto::{Codec, CodecSet, Hello, HelloAck};
use fernsicht_secure::session::{Responder, Transport};

use crate::HostAgent;
use crate::audio::{audio_loop, make_audio};
use crate::backends::{default_bitrate_kbps, fit, frame_rate, make_encoder, make_source};
use crate::capture::capture_loop;
use crate::config::{AudioKind, CaptureKind, EncoderKind, SessionParams};
use crate::encode::{Rebuild, encode_loop};
use crate::health::{RepeatedError, StopOnExit};
use crate::input::make_input;
use crate::link::{Link, UdpLink};
use crate::monitors::MonitorSender;
use crate::pointer::CursorSender;
use crate::send::send_loop;
use crate::session::{Session, codec_label, new_session_id};
use crate::shared::{HostStats, Shared};
use crate::web::WebOut;

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
            // Only NVENC refreshes in this build of the encoders.
            refresh: hello.refresh && matches!(self.cfg.encoder, EncoderKind::Nvenc { .. }),
            ten_bit: hello.ten_bit.and(hello.codecs),
            // Known once the screen is opened.
            screen: Colour::SDR,
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
        let shared = Arc::new(new_shared(&self.stats, &params, &loss));
        // Source and encoder come first: if the screen or the GPU is
        // unavailable the session fails before any thread starts.
        let Backends {
            source,
            encoder,
            params,
            bounds,
        } = self.backends(params)?;
        let codec = encoder.codec();
        let gpu = self.gpu_boost();
        shared
            .target_kbps
            .store(params.bitrate_kbps, Ordering::Relaxed);
        shared.set_screen(source.screen());
        let mut input = make_input(&self.cfg.input, &shared);
        let (crypto, handshake_reply) = match secure {
            None => (None, None),
            Some(responder) => {
                let (t, reply) = self.answer_handshake(responder, &params, codec)?;
                (Some(t), Some(reply))
            }
        };
        let web = web.map(|setup| (setup, crate::web::out_channel()));
        let web_tx = web.as_ref().map(|(_, (tx, _))| tx);
        let link = link_for(&self.socket, peer, params.session_id, crypto, web_tx);
        if let (Some(input), Some(out)) = (
            input.as_mut(),
            crate::input::rumble_out(&link, params.session_id),
        ) {
            input.on_rumble(out);
        }
        let frame_slot = Arc::new(Slot::new());
        let pipe = Pipe {
            shared: &shared,
            slot: &frame_slot,
            link: &link,
            params,
        };
        let is_web = web.is_some();
        let threads =
            self.spawn_threads(&pipe, (source, encoder, bounds), codec, web, &mut input)?;

        let session = Session {
            params,
            codec,
            client: None,
            peer,
            web: is_web,
            link,
            handshake: None,
            last_seen: Instant::now(),
            shared,
            loss,
            rate: RateController::new(params.bitrate_kbps, Instant::now()),
            // The counter spans all sessions: this one starts from here.
            overflows_seen: HostStats::get(&self.stats.send_overflows),
            client_rtt: Duration::ZERO,
            threads,
            frame_slot,
            input,
            input_seen: Dedup::default(),
            input_errors: RepeatedError::default(),
            _gpu: gpu,
        };
        Ok((session, handshake_reply))
    }

    /// Starts the session's threads: capture, encode, sound, send and, for
    /// a browser, WebRTC. If one fails to start, those started already are
    /// stopped.
    fn spawn_threads(
        &self,
        pipe: &Pipe<'_>,
        (source, encoder, bounds): (Box<dyn FrameSource>, Box<dyn Encoder>, (u16, u16)),
        codec: Codec,
        web: Option<(crate::web::WebSetup, WebChannel)>,
        input: &mut Option<Box<dyn InputSink>>,
    ) -> anyhow::Result<Vec<JoinHandle<()>>> {
        let (send_tx, send_rx) = bounded::<EncodedFrame>(SEND_QUEUE);
        let free_frames = filled(BUFFERS, || source.alloc_frame());
        let free_enc = filled(ENCODED_BUFFERS, EncodedFrame::default);
        // Threads started so far; stopped and joined if a later one fails.
        let mut threads = {
            let (shared, slot) = (pipe.shared.clone(), pipe.slot.clone());
            crate::health::Starting::new(move || {
                shared.running.store(false, Ordering::Release);
                slot.close();
            })
        };
        threads.add(spawn_capture(pipe, source, free_frames.clone())?);
        let rebuild = rebuild_for(self.cfg.encoder.clone(), pipe.params);
        let encoding = (encoder, rebuild, bounds);
        threads.add(spawn_encode(
            pipe,
            encoding,
            send_tx,
            &free_frames.0,
            free_enc.clone(),
        )?);
        if let Some(t) = spawn_audio(pipe, self.cfg.audio)? {
            threads.add(t);
        }
        let out = web
            .as_ref()
            .map(|(_, (tx, _))| (tx.clone(), codec_label(codec)));
        let pacing = (self.cfg.pace_bytes_per_sec, self.cfg.loss);
        threads.add(spawn_send(pipe, send_rx, free_enc.0, out, pacing)?);
        // The browser's input arrives on the WebRTC thread, which injects
        // it there (the host's loop only polls every 100 ms).
        if let Some((setup, (_, rx))) = web {
            let shared = pipe.shared.clone();
            threads.add(crate::web::spawn(setup, codec, shared, rx, input.take())?);
        }
        Ok(threads.done())
    }

    /// The session's picture source and encoder, and its parameters as they
    /// turn out: the frame rate the monitor shows, the host's own size for
    /// a client that left it to the host, the codec the encoder makes.
    fn backends(&self, params: SessionParams) -> anyhow::Result<Backends> {
        let source = make_source(&self.cfg.capture, &params)?;
        let bounds = self.bounds(&params);
        // Before the default bitrate is worked out per frame.
        let params = SessionParams {
            fps: frame_rate(params.fps, source.refresh_hz()),
            screen: source.colour(),
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
        // Rebuilt encoders (new bitrate) keep the codec the client was told.
        let params = SessionParams {
            codecs: CodecSet::EMPTY.with(encoder.codec()),
            ..params
        };
        Ok(Backends {
            source,
            encoder,
            params,
            bounds,
        })
    }

    /// The encoder's GPU at full clocks while the session runs.
    fn gpu_boost(&self) -> Option<crate::power::GpuBoost> {
        match &self.cfg.encoder {
            EncoderKind::Vaapi { render_node } | EncoderKind::Vulkan { render_node }
                if self.cfg.settings.gpu_boost() =>
            {
                let plan = crate::power::plan(Path::new("/sys"), render_node);
                Some(crate::power::GpuBoost::apply(
                    plan,
                    self.cfg.power_record.clone(),
                ))
            }
            _ => None,
        }
    }

    /// Answers the client's handshake with the HelloAck inside: the
    /// session's keys and the answer to send.
    fn answer_handshake(
        &self,
        responder: Responder,
        params: &SessionParams,
        codec: Codec,
    ) -> anyhow::Result<(Arc<Transport>, Vec<u8>)> {
        let mut ack = [0u8; HelloAck::LEN];
        self.hello_ack(params, codec).encode(&mut ack);
        let (transport, reply) = responder
            .reply(&ack)
            .map_err(|e| anyhow::anyhow!("handshake: {e}"))?;
        Ok((Arc::new(transport), reply))
    }

    pub(crate) fn hello_ack(&self, p: &SessionParams, codec: Codec) -> HelloAck {
        HelloAck {
            session_id: p.session_id,
            width: p.width,
            height: p.height,
            fps: p.fps,
            codec,
            refresh: p.refresh,
        }
    }
}

/// Where a session's picture comes from and what encodes it, with the
/// session's parameters as they turned out and the box a monitor's picture
/// is fitted into.
struct Backends {
    source: Box<dyn FrameSource>,
    encoder: Box<dyn Encoder>,
    params: SessionParams,
    bounds: (u16, u16),
}

/// The channel from a web session's threads to its WebRTC thread.
type WebChannel = (Sender<WebOut>, Receiver<WebOut>);

/// The way to the client: a browser's WebRTC thread, or our protocol to
/// `peer`, sealed with `crypto` when the session is secure.
fn link_for(
    socket: &Arc<UdpSocket>,
    peer: SocketAddr,
    session_id: u32,
    crypto: Option<Arc<Transport>>,
    web: Option<&Sender<WebOut>>,
) -> Link {
    match web {
        Some(tx) => Link::Web(tx.clone()),
        None => Link::Udp(UdpLink {
            socket: socket.clone(),
            peer,
            session_id,
            // A plain session (tests) has no keys to prove.
            confirmed: Arc::new(AtomicBool::new(crypto.is_none())),
            gso: Arc::new(AtomicBool::new(true)),
            crypto,
        }),
    }
}

/// A free list of `n` buffers made by `make`.
fn filled<T>(n: usize, make: impl Fn() -> T) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = bounded(n);
    for _ in 0..n {
        tx.send(make()).expect("room for all");
    }
    (tx, rx)
}

/// What every thread of a session gets.
struct Pipe<'a> {
    shared: &'a Arc<Shared>,
    /// Captured frames for the encoder, the latest wins.
    slot: &'a Arc<Slot<Frame>>,
    link: &'a Link,
    params: SessionParams,
}

fn new_shared(stats: &Arc<HostStats>, params: &SessionParams, loss: &LossEstimator) -> Shared {
    Shared {
        stats: stats.clone(),
        running: AtomicBool::new(true),
        keyframe_requested: AtomicBool::new(true),
        last_keyframe: Mutex::new(None),
        frame: Duration::from_micros(clock::frame_interval_us(u32::from(params.fps))),
        cursor_requested: AtomicBool::new(false),
        fec_loss: AtomicU32::new(loss.estimate().to_bits()),
        target_kbps: AtomicU32::new(0),
        delivery_kbps: AtomicU32::new(0),
        // A few frames' worth; more waiting means the sender is behind,
        // and then they come too late anyway.
        nacks: crossbeam_channel::bounded(16),
        monitor_request: Default::default(),
        screen: Default::default(),
        screen_changes: AtomicU64::new(0),
        pointer: AtomicU64::new(crate::shared::NO_POINTER),
    }
}

/// A new encoder at another bitrate (for encoders that cannot change it
/// while running) or size (another monitor).
fn rebuild_for(kind: EncoderKind, params: SessionParams) -> Rebuild {
    Box::new(move |kbps, (width, height), screen| {
        make_encoder(
            &kind,
            &SessionParams {
                bitrate_kbps: kbps,
                width,
                height,
                screen,
                ..params
            },
        )
    })
}

/// Frames from the source into the slot; the pointer and the monitor list
/// go out from here too.
fn spawn_capture(
    pipe: &Pipe<'_>,
    source: Box<dyn FrameSource>,
    (free_tx, free_rx): (Sender<Frame>, Receiver<Frame>),
) -> std::io::Result<JoinHandle<()>> {
    let (shared, slot) = (pipe.shared.clone(), pipe.slot.clone());
    let cursor = CursorSender::new(
        pipe.link.clone(),
        pipe.params.session_id,
        (source.width(), source.height()),
    );
    let monitors = MonitorSender::new(pipe.link.clone());
    spawn_hot("capture", move || {
        let _guard = StopOnExit {
            running: &shared.running,
            thread: "capture",
        };
        capture_loop(source, &shared, &slot, &free_rx, &free_tx, cursor, monitors);
    })
}

/// Frames from the slot through the encoder to the send queue.
fn spawn_encode(
    pipe: &Pipe<'_>,
    (encoder, rebuild, bounds): (Box<dyn Encoder>, Rebuild, (u16, u16)),
    send_tx: Sender<EncodedFrame>,
    free_frames: &Sender<Frame>,
    (free_enc_tx, free_enc_rx): (Sender<EncodedFrame>, Receiver<EncodedFrame>),
) -> std::io::Result<JoinHandle<()>> {
    let (shared, slot) = (pipe.shared.clone(), pipe.slot.clone());
    let (params, free_frames) = (pipe.params, free_frames.clone());
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
            params.screen,
            &shared,
            &slot,
            &send_tx,
            &free_frames,
            &free_enc_rx,
            &free_enc_tx,
        );
    })
}

/// The host's sound, if the configuration has some and it opens.
fn spawn_audio(pipe: &Pipe<'_>, kind: AudioKind) -> std::io::Result<Option<JoinHandle<()>>> {
    let source = match make_audio(kind) {
        Ok(Some(source)) => source,
        Ok(None) => return Ok(None),
        Err(e) => {
            log::warn!("no sound: {e}");
            return Ok(None);
        }
    };
    let (shared, link) = (pipe.shared.clone(), pipe.link.clone());
    let session_id = pipe.params.session_id;
    spawn_hot("audio", move || {
        if let Err(e) = audio_loop(source, &shared, &link, session_id) {
            log::warn!("audio: {e}");
        }
    })
    .map(Some)
}

/// Encoded frames to the client: to a browser's WebRTC thread (`web`, with
/// the codec's name for its overlay), or paced (bytes per second) over our
/// protocol with simulated loss.
fn spawn_send(
    pipe: &Pipe<'_>,
    queue: Receiver<EncodedFrame>,
    free_enc: Sender<EncodedFrame>,
    web: Option<(Sender<crate::web::WebOut>, &'static str)>,
    (pace_bytes_per_sec, loss): (u64, f64),
) -> std::io::Result<JoinHandle<()>> {
    let (shared, link, params) = (pipe.shared.clone(), pipe.link.clone(), pipe.params);
    let pacer = Pacer::new(pace_bytes_per_sec, pacer::BURST_BYTES);
    let loss = LossSim::new(loss, u64::from(params.session_id) | 1);
    spawn_hot("send", move || {
        let _guard = StopOnExit {
            running: &shared.running,
            thread: "send",
        };
        match web {
            Some((out, label)) => {
                crate::web::send_loop(&shared, &queue, &free_enc, &out, params, label);
            }
            None => {
                if let Err(e) = send_loop(&shared, &queue, &free_enc, &link, params, pacer, loss) {
                    log::error!("send: {e:#}");
                }
            }
        }
    })
}
