//! The web viewer: the host serves the viewer page and runs WebRTC
//! sessions for browsers.
//!
//! ```text
//! browser ── GET /            ──► viewer page (web/viewer, built)
//!         ── GET /api/info    ──► host name, pairing open?
//!         ── POST /api/connect {pin, offer} ──► host loop: PIN, session ──► {answer}
//!         ◄═ WebRTC (UDP) ═══ H.264 video, Opus sound, data channel:
//!                              pointer (proto packets) and stats → browser,
//!                              mouse and keyboard (JSON) → host
//! ```
//!
//! Browsers reach the page over plain `http://` in the LAN, where only
//! WebRTC of the browser media APIs works (WebCodecs and WebTransport need
//! HTTPS). Picture, sound and input are encrypted by WebRTC (DTLS-SRTP).
//! A browser is let in with the PIN the host shows for pairing; it is
//! used once and not remembered. The PIN itself crosses the LAN in the
//! clear, like the page.

use std::io::Read;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, select};
use fernsicht_codec::EncodedFrame;
use fernsicht_input::InputSink;
use fernsicht_proto::{
    BTN_PAD_FIRST, BTN_PAD_LAST, Codec, CodecSet, InputEvent, MAX_PADS, PadAxis,
};
use serde_json::{Value, json};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::channel::ChannelId;
use str0m::format::Codec as RtcCodec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};

use crate::{HostSecurity, HostStats, SessionParams, Shared};

/// What a session sends to the browser, from its threads to the WebRTC
/// thread.
pub(crate) enum WebOut {
    /// A proto packet (pointer) for the data channel.
    Packet(Vec<u8>),
    Video {
        data: Vec<u8>,
        capture_us: u64,
    },
    /// One Opus frame of [`fernsicht_audio::FRAME_SAMPLES`] samples.
    Audio {
        data: Vec<u8>,
    },
    /// Text for the data channel (JSON).
    Text(String),
}

/// Messages queued for the WebRTC thread before the sender waits.
const OUT_QUEUE: usize = 64;

/// A browser asking for a session, from the HTTP thread to the host's loop.
pub(crate) struct WebRequest {
    pub pin: String,
    pub offer: SdpOffer,
    pub browser: SocketAddr,
    /// Our address on the browser's network: the WebRTC candidate.
    pub local_ip: IpAddr,
    pub reply: Sender<Result<WebAccepted, WebError>>,
}

pub(crate) struct WebAccepted {
    pub answer: SdpAnswer,
    pub params: SessionParams,
}

/// Why a browser is turned away (the viewer shows it in words).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebError {
    WrongPin,
    PairingClosed,
    TooManyAttempts,
    /// The session could not start (screen, GPU, WebRTC).
    Failed,
}

impl WebError {
    fn code(self) -> &'static str {
        match self {
            WebError::WrongPin => "wrong-pin",
            WebError::PairingClosed => "pairing-closed",
            WebError::TooManyAttempts => "too-many-attempts",
            WebError::Failed => "failed",
        }
    }

    fn status(self) -> u16 {
        match self {
            WebError::Failed => 500,
            _ => 403,
        }
    }
}

/// The WebRTC side of a new session: answer sent, waiting for the browser.
pub(crate) struct WebSetup {
    rtc: Rtc,
    socket: UdpSocket,
    browser_codecs: CodecSet,
}

impl WebSetup {
    /// The video codecs the browser offered that we answered with.
    pub(crate) fn browser_codecs(&self) -> CodecSet {
        self.browser_codecs
    }
}

/// The video codecs a browser's offer lists that our encoders make: H.264,
/// and AV1, which Chrome and Firefox decode even without a hardware decoder.
/// HEVC is left out: few browsers take it over WebRTC.
pub(crate) fn codecs_in_offer(sdp: &str) -> CodecSet {
    let has = |name: &str| {
        sdp.lines().any(|l| {
            l.strip_prefix("a=rtpmap:")
                .and_then(|rest| rest.split_once(' '))
                .is_some_and(|(_, enc)| enc.eq_ignore_ascii_case(name))
        })
    };
    let mut set = CodecSet::EMPTY;
    if has("H264/90000") {
        set = set.with(Codec::H264);
    }
    if has("AV1/90000") {
        set = set.with(Codec::Av1);
    }
    set
}

/// H.264 as our encoders make it: High profile, and Constrained Baseline
/// for browsers that offer nothing else. (pt, rtx, profile-level-id)
const H264: [(u8, u8, u32); 2] = [(114, 115, 0x64_00_1f), (108, 109, 0x42_e0_1f)];

/// Prepares WebRTC for `offer`: a UDP socket on `local_ip` and our answer.
pub(crate) fn accept(offer: SdpOffer, local_ip: IpAddr) -> anyhow::Result<(WebSetup, SdpAnswer)> {
    static CRYPTO: Once = Once::new();
    CRYPTO.call_once(|| str0m::crypto::from_feature_flags().install_process_default());
    let browser_codecs = codecs_in_offer(&offer.to_sdp_string());
    let mut cfg = RtcConfig::new()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_opus(true, false)
        .enable_av1(true);
    for (pt, rtx, profile) in H264 {
        cfg.codec_config()
            .add_h264(pt.into(), Some(rtx.into()), true, profile);
    }
    let mut rtc = cfg.build(Instant::now());
    let socket = UdpSocket::bind((local_ip, 0))?;
    let candidate = Candidate::host(socket.local_addr()?, "udp")
        .map_err(|e| anyhow::anyhow!("candidate: {e}"))?;
    rtc.add_local_candidate(candidate);
    let answer = rtc
        .sdp_api()
        .accept_offer(offer)
        .map_err(|e| anyhow::anyhow!("offer: {e}"))?;
    Ok((
        WebSetup {
            rtc,
            socket,
            browser_codecs,
        },
        answer,
    ))
}

/// Our address towards `peer` (the route the kernel would take).
pub(crate) fn local_ip_towards(peer: IpAddr) -> std::io::Result<IpAddr> {
    let s = UdpSocket::bind((
        if peer.is_ipv4() {
            IpAddr::from([0u8; 4])
        } else {
            IpAddr::from([0u16; 8])
        },
        0,
    ))?;
    s.connect((peer, 9))?;
    Ok(s.local_addr()?.ip())
}

/// Starts the WebRTC thread of a session. It ends when the browser goes
/// away (then it clears `shared.running`, which ends the session) or the
/// session ends.
pub(crate) fn spawn(
    setup: WebSetup,
    codec: Codec,
    shared: Arc<Shared>,
    out: Receiver<WebOut>,
    input: Option<Box<dyn InputSink>>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("webrtc".into())
        .spawn(move || {
            let mut s = RtcSession::new(setup, codec, shared.clone(), input);
            if let Err(e) = s.run(&out) {
                log::warn!("web session: {e:#}");
            }
            s.end();
        })
}

struct RtcSession {
    rtc: Rtc,
    socket: UdpSocket,
    shared: Arc<Shared>,
    input: Option<Box<dyn InputSink>>,
    connected: bool,
    /// What the encoder makes: H.264 or AV1.
    codec: Codec,
    video: Option<(Mid, Pt)>,
    audio: Option<(Mid, Pt)>,
    channel: Option<ChannelId>,
    /// RTP time of the next Opus frame.
    audio_time: u64,
}

impl RtcSession {
    fn new(
        setup: WebSetup,
        codec: Codec,
        shared: Arc<Shared>,
        input: Option<Box<dyn InputSink>>,
    ) -> Self {
        Self {
            rtc: setup.rtc,
            socket: setup.socket,
            shared,
            input,
            connected: false,
            codec,
            video: None,
            audio: None,
            channel: None,
            audio_time: 0,
        }
    }

    fn run(&mut self, out: &Receiver<WebOut>) -> anyhow::Result<()> {
        let local = self.socket.local_addr()?;
        let net = receive_thread(self.socket.try_clone()?, self.shared.clone())?;
        let result = (|| {
            while self.shared.running.load(Ordering::Acquire) {
                let deadline = match self.poll()? {
                    Some(t) => t,
                    None => return Ok(()),
                };
                let now = Instant::now();
                if deadline <= now {
                    self.rtc.handle_input(Input::Timeout(now))?;
                    continue;
                }
                let wait = (deadline - now).min(Duration::from_millis(100));
                select! {
                    recv(net.1) -> m => {
                        let Ok((at, source, data)) = m else { return Ok(()) };
                        let Ok(contents) = data.as_slice().try_into() else { continue };
                        self.rtc.handle_input(Input::Receive(at, Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: local,
                            contents,
                        }))?;
                    }
                    recv(out) -> m => match m {
                        Ok(m) => self.send(m),
                        // The session's threads are gone.
                        Err(_) => return Ok(()),
                    },
                    default(wait) => self.rtc.handle_input(Input::Timeout(Instant::now()))?,
                }
            }
            Ok(())
        })();
        self.shared.running.store(false, Ordering::Release);
        let _ = net.0.join();
        result
    }

    /// Sends what str0m has to send and handles its events; returns when
    /// it wants to be woken next, `None` when the browser is gone.
    fn poll(&mut self) -> anyhow::Result<Option<Instant>> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(t) => return Ok(Some(t)),
                Output::Transmit(t) => {
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Output::Event(e) => {
                    if !self.on_event(e) {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// Returns false when the session is over.
    fn on_event(&mut self, e: Event) -> bool {
        match e {
            Event::Connected => {
                log::info!("web session: browser connected");
                self.connected = true;
                self.shared
                    .keyframe_requested
                    .store(true, Ordering::Relaxed);
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                log::info!("web session: browser gone");
                return false;
            }
            Event::MediaAdded(m) => {
                let Some(writer) = self.rtc.writer(m.mid) else {
                    return true;
                };
                let params: Vec<_> = writer.payload_params().cloned().collect();
                match m.kind {
                    MediaKind::Video => {
                        self.video = pick_video(&params, self.codec).map(|pt| (m.mid, pt));
                    }
                    MediaKind::Audio => {
                        self.audio = params
                            .iter()
                            .find(|p| p.spec().codec == RtcCodec::Opus)
                            .map(|p| (m.mid, p.pt()));
                    }
                }
            }
            Event::KeyframeRequest(_) => {
                self.shared
                    .keyframe_requested
                    .store(true, Ordering::Relaxed);
            }
            Event::ChannelOpen(id, _) => {
                self.channel = Some(id);
                self.shared.cursor_requested.store(true, Ordering::Relaxed);
            }
            Event::ChannelData(d) if !d.binary => match parse_input(&d.data) {
                Some(Command::Event(event)) => {
                    if let Some(sink) = self.input.as_mut()
                        && let Err(e) = sink.inject(&event)
                    {
                        log::debug!("web input: {e}");
                    }
                }
                Some(Command::ReleaseAll) => {
                    if let Some(sink) = self.input.as_mut() {
                        sink.release_all();
                    }
                }
                Some(Command::Keyframe) => {
                    self.shared
                        .keyframe_requested
                        .store(true, Ordering::Relaxed);
                }
                Some(Command::Bye) => {
                    log::info!("web session: browser said goodbye");
                    return false;
                }
                None => log::debug!("web input: unreadable message"),
            },
            // The page went away (closed, reloaded): no need to wait for
            // the connection to time out.
            Event::ChannelClose(_) => {
                log::info!("web session: data channel closed");
                return false;
            }
            _ => {}
        }
        true
    }

    fn send(&mut self, m: WebOut) {
        if !self.connected {
            return;
        }
        match m {
            WebOut::Video { data, capture_us } => {
                let Some((mid, pt)) = self.video else { return };
                if let Some(w) = self.rtc.writer(mid) {
                    let time = MediaTime::new(capture_us * 9 / 100, Frequency::NINETY_KHZ);
                    if let Err(e) = w.write(pt, Instant::now(), time, data) {
                        log::debug!("web video: {e}");
                    }
                }
            }
            WebOut::Audio { data } => {
                let Some((mid, pt)) = self.audio else { return };
                if let Some(w) = self.rtc.writer(mid) {
                    let time = MediaTime::new(self.audio_time, Frequency::FORTY_EIGHT_KHZ);
                    let _ = w.write(pt, Instant::now(), time, data);
                }
                self.audio_time += fernsicht_audio::FRAME_SAMPLES as u64;
            }
            WebOut::Packet(p) => self.write_channel(true, &p),
            WebOut::Text(t) => self.write_channel(false, t.as_bytes()),
        }
    }

    fn write_channel(&mut self, binary: bool, data: &[u8]) {
        if let Some(mut c) = self.channel.and_then(|id| self.rtc.channel(id)) {
            // A full buffer drops the message: the pointer is sent again
            // with the next frame, stats every second.
            let _ = c.write(binary, data);
        }
    }

    fn end(&mut self) {
        if let Some(mut sink) = self.input.take() {
            sink.release_all();
        }
        self.rtc.disconnect();
        self.shared.running.store(false, Ordering::Release);
    }
}

/// Datagrams from the socket, read on their own thread so that the WebRTC
/// thread can wait on them and on the session's output at once.
type Datagram = (Instant, SocketAddr, Vec<u8>);

fn receive_thread(
    socket: UdpSocket,
    shared: Arc<Shared>,
) -> std::io::Result<(JoinHandle<()>, Receiver<Datagram>)> {
    let (tx, rx) = bounded::<Datagram>(256);
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let t = std::thread::Builder::new()
        .name("webrtc-recv".into())
        .spawn(move || {
            let mut buf = vec![0u8; 2048];
            while shared.running.load(Ordering::Acquire) {
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if tx.send((Instant::now(), from, buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(e) => {
                        log::debug!("web socket: {e}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        })?;
    Ok((t, rx))
}

/// The payload type to send `codec` with. For H.264: High if the browser
/// takes it, else any packetization-mode-1 H.264.
fn pick_video(params: &[str0m::format::PayloadParams], codec: Codec) -> Option<Pt> {
    if codec == Codec::Av1 {
        return params
            .iter()
            .find(|p| p.spec().codec == RtcCodec::Av1)
            .map(|p| p.pt());
    }
    let h264 = |p: &&str0m::format::PayloadParams| {
        let s = p.spec();
        s.codec == RtcCodec::H264 && s.format.packetization_mode == Some(1)
    };
    let profile =
        |p: &str0m::format::PayloadParams| p.spec().format.profile_level_id.map(|v| v >> 16);
    params
        .iter()
        .filter(h264)
        .find(|p| profile(p) == Some(0x64))
        .or_else(|| params.iter().find(h264))
        .map(|p| p.pt())
}

/// What the browser sends over the data channel.
#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Event(InputEvent),
    /// Let go of everything (the page lost focus).
    ReleaseAll,
    Keyframe,
    /// The browser ends the session.
    Bye,
}

/// Parses one input message:
///
/// ```text
/// {"t":"m","x":0..65535,"y":0..65535}   pointer at a position
/// {"t":"r","dx":..,"dy":..}             pointer moved (captured)
/// {"t":"b","c":272,"p":true}            button (Linux BTN_* code)
/// {"t":"w","dx":0,"dy":120}             wheel, 120 per notch
/// {"t":"k","c":30,"p":true}             key (Linux KEY_* code)
/// {"t":"pb","n":0,"c":304,"p":true}     gamepad n's button (BTN_SOUTH..)
/// {"t":"pa","n":0,"a":0,"v":-32768}     gamepad n's axis (ABS_* code)
/// {"t":"release"}  {"t":"keyframe"}  {"t":"bye"}
/// ```
///
/// Anything out of range is refused, as in the native protocol.
pub(crate) fn parse_input(data: &[u8]) -> Option<Command> {
    let v: Value = serde_json::from_slice(data).ok()?;
    let int = |k: &str, lo: i64, hi: i64| v.get(k)?.as_i64().filter(|n| (lo..=hi).contains(n));
    let pressed = || v.get("p")?.as_bool();
    const MOVE: i64 = 1 << 16;
    const WHEEL: i64 = 120 * 100;
    let event = match v.get("t")?.as_str()? {
        "m" => InputEvent::MouseAbs {
            x: int("x", 0, 65535)? as u16,
            y: int("y", 0, 65535)? as u16,
        },
        "r" => InputEvent::MouseRel {
            dx: int("dx", -MOVE, MOVE)? as i32,
            dy: int("dy", -MOVE, MOVE)? as i32,
        },
        "b" => InputEvent::Button {
            code: int("c", 0x110, 0x117)? as u16,
            pressed: pressed()?,
        },
        "w" => InputEvent::Scroll {
            dx: int("dx", -WHEEL, WHEEL)? as i32,
            dy: int("dy", -WHEEL, WHEEL)? as i32,
        },
        "k" => InputEvent::Key {
            code: int("c", 1, 0x2ff)? as u16,
            pressed: pressed()?,
        },
        "pb" => InputEvent::PadButton {
            pad: int("n", 0, i64::from(MAX_PADS) - 1)? as u8,
            code: int("c", i64::from(BTN_PAD_FIRST), i64::from(BTN_PAD_LAST))? as u16,
            pressed: pressed()?,
        },
        "pa" => {
            let axis = PadAxis::from_code(int("a", 0, 0xff)? as u16)?;
            let r = axis.range();
            InputEvent::PadAxis {
                pad: int("n", 0, i64::from(MAX_PADS) - 1)? as u8,
                axis,
                value: int("v", i64::from(*r.start()), i64::from(*r.end()))? as i32,
            }
        }
        "release" => return Some(Command::ReleaseAll),
        "keyframe" => return Some(Command::Keyframe),
        "bye" => return Some(Command::Bye),
        _ => return None,
    };
    Some(Command::Event(event))
}

/// Forwards a web session's encoded frames to the WebRTC thread, with the
/// host's share of the overlay once per second.
pub(crate) fn send_loop(
    shared: &Shared,
    queue: &Receiver<EncodedFrame>,
    free_enc: &Sender<EncodedFrame>,
    out: &Sender<WebOut>,
    params: SessionParams,
    codec: &'static str,
) {
    let mut window = Instant::now();
    let (mut frames, mut bytes, mut capture_us, mut encode_us) = (0u64, 0u64, 0u64, 0u64);
    while let Ok(enc) = queue.recv() {
        if !shared.running.load(Ordering::Acquire) {
            break;
        }
        frames += 1;
        bytes += enc.data.len() as u64;
        capture_us += enc.capture_ready_us.saturating_sub(enc.capture_us);
        encode_us += enc.encoded_us.saturating_sub(enc.capture_ready_us);
        let video = WebOut::Video {
            data: enc.data.clone(),
            capture_us: enc.capture_us,
        };
        let _ = free_enc.send(enc);
        if out.send(video).is_err() {
            break;
        }
        HostStats::bump(&shared.stats.frames_sent);
        let elapsed = window.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            let stats = json!({
                "type": "stats",
                "captureUs": capture_us / frames,
                "encodeUs": encode_us / frames,
                "fps": frames as f64 / secs,
                "bitrateBps": (bytes as f64 * 8.0 / secs) as u64,
                "width": params.width,
                "height": params.height,
                "codec": codec,
            });
            let _ = out.try_send(WebOut::Text(stats.to_string()));
            window = Instant::now();
            (frames, bytes, capture_us, encode_us) = (0, 0, 0, 0);
        }
    }
}

/// A new channel for a web session's output.
pub(crate) fn out_channel() -> (Sender<WebOut>, Receiver<WebOut>) {
    bounded(OUT_QUEUE)
}

// ── HTTP ────────────────────────────────────────────────────────────────

/// Largest request body read (an SDP offer is a few KiB).
const MAX_BODY: u64 = 64 * 1024;

/// Serves the viewer page from `root` and the API until `stop`.
pub(crate) fn serve(
    addr: &str,
    root: Option<PathBuf>,
    sec: Arc<HostSecurity>,
    requests: Sender<WebRequest>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<(JoinHandle<()>, SocketAddr)> {
    let server =
        tiny_http::Server::http(addr).map_err(|e| anyhow::anyhow!("web server on {addr}: {e}"))?;
    let local = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow::anyhow!("web server: not an IP address"))?;
    let server = Arc::new(server);
    let t = std::thread::Builder::new()
        .name("web".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match server.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(req)) => {
                        // Connecting waits for the session to start; pages
                        // and the other API calls must not wait behind it.
                        let (root, sec, requests) = (root.clone(), sec.clone(), requests.clone());
                        std::thread::spawn(move || {
                            handle(req, root.as_deref(), &sec, &requests);
                        });
                    }
                    Ok(None) => {}
                    Err(e) => log::warn!("web server: {e}"),
                }
            }
        })?;
    Ok((t, local))
}

fn json_response(status: u16, v: &Value) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    tiny_http::Response::from_data(v.to_string().into_bytes())
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json"))
        .with_header(header("Cache-Control", "no-store"))
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn handle(
    mut req: tiny_http::Request,
    root: Option<&Path>,
    sec: &HostSecurity,
    requests: &Sender<WebRequest>,
) {
    let path = req.url().split('?').next().unwrap_or("/").to_owned();
    let response = match (req.method(), path.as_str()) {
        (tiny_http::Method::Get, "/api/info") => json_response(
            200,
            &json!({
                "name": sec.name(),
                "pairing": sec.pairing_remaining().is_some(),
            }),
        ),
        (tiny_http::Method::Post, "/api/connect") => {
            let mut body = Vec::new();
            let read = req
                .as_reader()
                .take(MAX_BODY)
                .read_to_end(&mut body)
                .is_ok();
            let browser = req.remote_addr().copied();
            match (read, browser) {
                (true, Some(browser)) => connect(&body, browser, requests),
                _ => json_response(400, &json!({"error": "bad-request"})),
            }
        }
        (tiny_http::Method::Get | tiny_http::Method::Head, _) => {
            return serve_file(req, root, &path);
        }
        _ => json_response(404, &json!({"error": "not-found"})),
    };
    let _ = req.respond(response);
}

/// How long a browser waits for the session to start.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

fn connect(
    body: &[u8],
    browser: SocketAddr,
    requests: &Sender<WebRequest>,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let bad = || json_response(400, &json!({"error": "bad-request"}));
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return bad();
    };
    let (Some(pin), Some(offer)) = (v.get("pin").and_then(Value::as_str), v.get("offer")) else {
        return bad();
    };
    let Ok(offer) = serde_json::from_value::<SdpOffer>(offer.clone()) else {
        return bad();
    };
    let Ok(local_ip) = local_ip_towards(browser.ip()) else {
        return json_response(500, &json!({"error": "failed"}));
    };
    let (reply, answer) = bounded(1);
    let req = WebRequest {
        pin: pin.to_owned(),
        offer,
        browser,
        local_ip,
        reply,
    };
    if requests.send(req).is_err() {
        return json_response(503, &json!({"error": "failed"}));
    }
    match answer.recv_timeout(CONNECT_TIMEOUT) {
        Ok(Ok(a)) => json_response(
            200,
            &json!({
                "answer": a.answer,
                "width": a.params.width,
                "height": a.params.height,
                "fps": a.params.fps,
            }),
        ),
        Ok(Err(e)) => json_response(e.status(), &json!({"error": e.code()})),
        Err(_) => json_response(504, &json!({"error": "failed"})),
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// The file under `root` for a URL path; app routes (no file extension)
/// get `index.html`. Never anything outside `root`.
pub(crate) fn resolve_file(root: &Path, url_path: &str) -> Option<PathBuf> {
    let rel = Path::new(url_path.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let file = root.join(rel);
    if file.is_file() {
        return Some(file);
    }
    rel.extension()
        .is_none()
        .then(|| root.join("index.html"))
        .filter(|f| f.is_file())
}

fn serve_file(req: tiny_http::Request, root: Option<&Path>, path: &str) {
    let file = root.and_then(|r| resolve_file(r, path));
    let Some(file) = file else {
        let msg = if root.is_none() {
            "Der Web-Viewer ist auf diesem Host nicht installiert (--web-root)."
        } else {
            "Nicht gefunden."
        };
        let _ = req.respond(
            tiny_http::Response::from_string(msg)
                .with_status_code(404)
                .with_header(header("Content-Type", "text/plain; charset=utf-8")),
        );
        return;
    };
    match std::fs::File::open(&file) {
        Ok(f) => {
            // Built assets carry a hash in their name: cache them for good.
            let cache = if file.parent().is_some_and(|p| p.ends_with("assets")) {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            let _ = req.respond(
                tiny_http::Response::from_file(f)
                    .with_header(header("Content-Type", content_type(&file)))
                    .with_header(header("Cache-Control", cache)),
            );
        }
        Err(_) => {
            let _ = req.respond(tiny_http::Response::empty(500));
        }
    }
}

/// Checks a browser's PIN against the open pairing; a right PIN is used
/// up (pairing closes).
pub(crate) fn check_pin(sec: &HostSecurity, pin: &str) -> Result<(), WebError> {
    match sec.pairing_attempt() {
        Ok(expected) => {
            let ok = expected.len() == pin.len()
                && expected
                    .bytes()
                    .zip(pin.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0;
            if ok {
                sec.close_pairing();
                Ok(())
            } else {
                Err(WebError::WrongPin)
            }
        }
        Err(fernsicht_proto::RejectReason::TooManyAttempts) => Err(WebError::TooManyAttempts),
        Err(_) => Err(WebError::PairingClosed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codecs_are_read_from_the_offer() {
        // Abridged from Chrome, Firefox and Safari offers.
        let chrome = "m=video 9 UDP/TLS/RTP/SAVPF 96 45 103\r\n\
                      a=rtpmap:96 VP8/90000\r\n\
                      a=rtpmap:45 AV1/90000\r\n\
                      a=rtpmap:103 H264/90000\r\n";
        assert_eq!(
            codecs_in_offer(chrome),
            CodecSet::of(&[Codec::H264, Codec::Av1])
        );
        let safari = "a=rtpmap:96 H264/90000\r\na=rtpmap:98 H265/90000\r\n";
        assert_eq!(codecs_in_offer(safari), CodecSet::H264);
        // Lower case, and a codec named only in an attribute: not offered.
        let odd = "a=rtpmap:41 av1/90000\r\na=fmtp:100 apt=AV1/90000\r\n";
        assert_eq!(codecs_in_offer(odd), CodecSet::of(&[Codec::Av1]));
        assert_eq!(codecs_in_offer(""), CodecSet::EMPTY);
    }

    #[test]
    fn input_messages() {
        let p = |s: &str| parse_input(s.as_bytes());
        assert_eq!(
            p(r#"{"t":"m","x":100,"y":65535}"#),
            Some(Command::Event(InputEvent::MouseAbs { x: 100, y: 65535 }))
        );
        assert_eq!(
            p(r#"{"t":"r","dx":-5,"dy":7}"#),
            Some(Command::Event(InputEvent::MouseRel { dx: -5, dy: 7 }))
        );
        assert_eq!(
            p(r#"{"t":"b","c":272,"p":true}"#),
            Some(Command::Event(InputEvent::Button {
                code: 272,
                pressed: true
            }))
        );
        assert_eq!(
            p(r#"{"t":"w","dx":0,"dy":-120}"#),
            Some(Command::Event(InputEvent::Scroll { dx: 0, dy: -120 }))
        );
        assert_eq!(
            p(r#"{"t":"k","c":30,"p":false}"#),
            Some(Command::Event(InputEvent::Key {
                code: 30,
                pressed: false
            }))
        );
        assert_eq!(
            p(r#"{"t":"pb","n":1,"c":304,"p":true}"#),
            Some(Command::Event(InputEvent::PadButton {
                pad: 1,
                code: 304,
                pressed: true
            }))
        );
        assert_eq!(
            p(r#"{"t":"pa","n":0,"a":5,"v":255}"#),
            Some(Command::Event(InputEvent::PadAxis {
                pad: 0,
                axis: PadAxis::RightTrigger,
                value: 255
            }))
        );
        for bad in [
            r#"{"t":"pb","n":4,"c":304,"p":true}"#,
            r#"{"t":"pb","n":0,"c":272,"p":true}"#,
            r#"{"t":"pa","n":0,"a":5,"v":256}"#,
            r#"{"t":"pa","n":0,"a":9,"v":0}"#,
        ] {
            assert_eq!(p(bad), None, "{bad}");
        }
        assert_eq!(p(r#"{"t":"release"}"#), Some(Command::ReleaseAll));
        assert_eq!(p(r#"{"t":"keyframe"}"#), Some(Command::Keyframe));
        assert_eq!(p(r#"{"t":"bye"}"#), Some(Command::Bye));
        // Out of range, missing, wrong type, unknown, not JSON.
        for bad in [
            r#"{"t":"m","x":70000,"y":0}"#,
            r#"{"t":"m","x":-1,"y":0}"#,
            r#"{"t":"b","c":30,"p":true}"#,
            r#"{"t":"k","c":0,"p":true}"#,
            r#"{"t":"k","c":1000,"p":true}"#,
            r#"{"t":"k","c":30}"#,
            r#"{"t":"k","c":"30","p":true}"#,
            r#"{"t":"w","dx":0,"dy":99999999}"#,
            r#"{"t":"x"}"#,
            r#"{}"#,
            "rm -rf",
        ] {
            assert_eq!(p(bad), None, "{bad}");
        }
    }

    #[test]
    fn files_stay_inside_the_root() {
        let root = std::env::temp_dir().join(format!("fernsicht-web-{}", std::process::id()));
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("index.html"), "<html>").unwrap();
        std::fs::write(root.join("assets/app-1234.js"), "x").unwrap();
        assert_eq!(resolve_file(&root, "/"), Some(root.join("index.html")));
        assert_eq!(
            resolve_file(&root, "/assets/app-1234.js"),
            Some(root.join("assets/app-1234.js"))
        );
        // App routes get the page; missing files with an extension do not.
        assert_eq!(
            resolve_file(&root, "/session/abc"),
            Some(root.join("index.html"))
        );
        assert_eq!(resolve_file(&root, "/assets/gone.js"), None);
        for evil in ["/../etc/passwd", "/assets/../../x", "/./index.html"] {
            assert_eq!(resolve_file(&root, evil), None, "{evil}");
        }
        // A doubled slash stays inside the root too.
        assert_eq!(
            resolve_file(&root, "//etc/passwd"),
            Some(root.join("index.html"))
        );
        assert_eq!(content_type(Path::new("a.js")), "text/javascript");
        assert_eq!(
            content_type(Path::new("a.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("a")), "application/octet-stream");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pins_are_checked_once() {
        let sec = HostSecurity::new(
            fernsicht_secure::Identity::generate(),
            "zentrale",
            fernsicht_secure::Trusted::default(),
            None,
        );
        assert_eq!(check_pin(&sec, "123456"), Err(WebError::PairingClosed));
        sec.open_pairing("123456");
        assert_eq!(check_pin(&sec, "654321"), Err(WebError::WrongPin));
        assert_eq!(check_pin(&sec, "12345"), Err(WebError::WrongPin));
        assert_eq!(check_pin(&sec, "123456"), Ok(()));
        assert_eq!(
            check_pin(&sec, "123456"),
            Err(WebError::PairingClosed),
            "used up"
        );
        sec.open_pairing("111111");
        for _ in 0..3 {
            assert_eq!(check_pin(&sec, "000000"), Err(WebError::WrongPin));
        }
        assert_eq!(check_pin(&sec, "111111"), Err(WebError::TooManyAttempts));
        assert_eq!(WebError::WrongPin.code(), "wrong-pin");
        assert_eq!(WebError::Failed.status(), 500);
    }

    #[test]
    fn the_route_to_localhost_is_localhost() {
        assert!(
            local_ip_towards("127.0.0.1".parse().unwrap())
                .unwrap()
                .is_loopback()
        );
    }
}
