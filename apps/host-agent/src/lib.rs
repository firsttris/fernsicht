//! Host agent: answers session requests and runs the streaming pipeline.
//!
//! ```text
//! [capture] --slot(1)--> [encode] --fifo(2)--> [packetize + FEC + pacing + send]
//! [control] Hello/HelloAck, clock pings, feedback → FEC + keyframes
//! ```
//!
//! Raw frames meet a latest-frame-wins [`Slot`](fernsicht_core::Slot): when the encoder is busy,
//! the newest capture replaces an unconsumed one, which costs nothing.
//! Encoded frames reference each other, so they go through a short FIFO and
//! are only dropped when it overflows; that forces a keyframe. Buffers
//! circulate through small free lists, so steady state does not allocate.

mod audio;
mod backends;
mod capture;
mod config;
pub mod control;
mod encode;
mod health;
mod http;
mod input;
mod layout;
mod limits;
mod link;
mod monitors;
mod pointer;
pub mod power;
mod requests;
mod security;
mod send;
mod session;
mod shared;
mod start;
mod still;
#[cfg(test)]
mod tests;
mod throughput;
mod web;

// The crate's public API, as before the split into modules.
pub use backends::{
    CODEC_PREFERENCE, TEST_PATTERN_SIZE, auto_encoder, choose_encoder, codec_candidates,
};
pub use config::{
    AudioKind, CaptureKind, EncoderKind, HostConfig, HostDescription, HostSettings, InputKind,
    SessionParams,
};
pub use security::{HostSecurity, PAIRING_ATTEMPTS, PAIRING_ATTEMPTS_TOTAL, PAIRING_OPEN_FOR};
pub use shared::HostStats;

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::{Receiver, bounded};
use fernsicht_core::now_us;
use fernsicht_core::thread::make_current_hot;
use fernsicht_net::socket;
use fernsicht_proto::{ClockPong, InputAck, MAX_DATAGRAM, Packet};

use health::End;
use security::PairingExchange;
use session::{Session, log_session, on_feedback, say_bye};

/// Discovery answers a second per asking address, and in total (each answer
/// is a little work; Discover is padded so answers never amplify).
const ANNOUNCE_PER_SOURCE: f64 = 20.0;
const ANNOUNCE_TOTAL: f64 = 200.0;

/// Handshakes a second per address and in total: each costs key
/// agreement (X25519) on the control loop, a real client sends one every
/// quarter second at most.
const HANDSHAKES_PER_SOURCE: f64 = 10.0;
const HANDSHAKES_TOTAL: f64 = 100.0;

/// Warnings about refused devices a second per address and in total.
const REFUSAL_LOGS_PER_SOURCE: f64 = 0.2;
const REFUSAL_LOGS_TOTAL: f64 = 2.0;

pub struct HostAgent {
    cfg: HostConfig,
    socket: Arc<UdpSocket>,
    stats: Arc<HostStats>,
    /// The current session, for the control socket.
    status: control::StatusCell,
    web: Option<WebServer>,
}

/// The web viewer's server: browsers' session requests come in here.
struct WebServer {
    addr: SocketAddr,
    requests: Receiver<web::WebRequest>,
    thread: JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

impl HostAgent {
    pub fn bind(cfg: HostConfig) -> anyhow::Result<Self> {
        let socket = socket::bind_udp(&cfg.bind).with_context(|| format!("bind {}", cfg.bind))?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        // Small and urgent by default (pointer, sound, answers); video
        // marks its own datagrams.
        if let Err(e) = socket::set_dscp(&socket, socket::Dscp::Interactive) {
            log::debug!("no DSCP marking: {e}");
        }
        // Clocks a crashed earlier run left raised go back first.
        if let Some(record) = &cfg.power_record {
            power::recover(record);
        }
        let web = match (&cfg.web, &cfg.security) {
            (Some(addr), Some(sec)) => {
                let (tx, requests) = bounded(4);
                let stop = Arc::new(AtomicBool::new(false));
                let (thread, addr) =
                    web::serve(addr, cfg.web_root.clone(), sec.clone(), tx, stop.clone())?;
                if cfg.web_root.is_none() {
                    log::info!("web viewer API on {addr} (no page: --web-root not set)");
                }
                Some(WebServer {
                    addr,
                    requests,
                    thread,
                    stop,
                })
            }
            _ => None,
        };
        Ok(Self {
            cfg,
            socket: Arc::new(socket),
            stats: Arc::default(),
            status: Arc::default(),
            web,
        })
    }

    /// Where the web viewer is served, if it is.
    pub fn web_addr(&self) -> Option<SocketAddr> {
        self.web.as_ref().map(|w| w.addr)
    }

    /// Counters that stay readable while and after [`run`](Self::run) runs.
    pub fn stats(&self) -> Arc<HostStats> {
        self.stats.clone()
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Serves sessions until `stop` is set.
    pub fn run(mut self, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
        // Input from the client is applied on this thread.
        make_current_hot("control");
        let mut buf = [0u8; 2048];
        let mut opened = [0u8; MAX_DATAGRAM];
        let mut out = [0u8; MAX_DATAGRAM];
        let mut session: Option<Session> = None;
        let mut pairing: Option<PairingExchange> = None;
        let mut announce_budget =
            limits::PerSource::new(ANNOUNCE_PER_SOURCE, ANNOUNCE_TOTAL, Instant::now());
        let mut handshake_budget =
            limits::PerSource::new(HANDSHAKES_PER_SOURCE, HANDSHAKES_TOTAL, Instant::now());
        let mut refusal_logs =
            limits::PerSource::new(REFUSAL_LOGS_PER_SOURCE, REFUSAL_LOGS_TOTAL, Instant::now());
        let control_stop = Arc::new(AtomicBool::new(false));
        let mut revocations_seen = self.cfg.security.as_ref().map_or(0, |s| s.revocations());
        let control = match (&self.cfg.control, &self.cfg.security) {
            (Some(path), Some(sec)) => {
                let t = control::serve(
                    path,
                    sec.clone(),
                    self.status.clone(),
                    self.cfg.settings.clone(),
                    control_stop.clone(),
                )
                .with_context(|| format!("control socket {}", path.display()))?;
                log::info!("control socket {}", path.display());
                Some(t)
            }
            _ => None,
        };

        while !stop.load(Ordering::Relaxed) {
            let revoked = self.cfg.security.as_ref().and_then(|sec| {
                let now = sec.revocations();
                (now != revocations_seen).then(|| {
                    revocations_seen = now;
                    sec
                })
            });
            // Taken every time: a request while no session runs must not
            // end the next one.
            let ended_here = self
                .cfg
                .security
                .as_ref()
                .is_some_and(|sec| sec.take_end_request());
            let end = session.as_ref().and_then(|s| {
                if ended_here {
                    return Some(End::EndedHere);
                }
                if let (Some(sec), Some(key)) = (revoked, s.client)
                    && sec.is_paired(&key).is_none()
                {
                    return Some(End::Unpaired);
                }
                health::session_end(
                    s.web,
                    s.shared.running.load(Ordering::Acquire),
                    s.last_seen.elapsed(),
                    self.cfg.client_timeout,
                )
            });
            if let Some(why) = end
                && let Some(s) = session.take()
            {
                self.end_session(s, why, &mut out);
            }
            if let Some(w) = &self.web {
                while let Ok(req) = w.requests.try_recv() {
                    self.on_web_request(req, &mut session);
                }
            }
            let (len, from) = match self.socket.recv_from(&mut buf) {
                Ok(r) => r,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    log::warn!("recv: {e}");
                    continue;
                }
            };
            let recv_us = now_us();
            let outer = match Packet::decode(&buf[..len]) {
                Ok(p) => p,
                Err(e) => {
                    log::debug!("{from}: dropping datagram: {e}");
                    continue;
                }
            };
            let from_peer = session.as_ref().is_some_and(|s| s.peer == from);
            // Sealed packets of the current session are opened and handled
            // like plain ones; they are the only authentic packets of a
            // secure session.
            let (packet, authentic) = match outer {
                Packet::Sealed(h, sealed) => {
                    let Some(s) = session.as_ref() else { continue };
                    if !from_peer || h.session_id != s.params.session_id {
                        continue;
                    }
                    let n = match s.link.open(h.counter, sealed, &mut opened) {
                        None => continue,
                        Some(Ok(n)) => n,
                        Some(Err(e)) => {
                            log::debug!("{from}: dropping sealed packet: {e}");
                            continue;
                        }
                    };
                    match Packet::decode(&opened[..n]) {
                        Ok(inner) => (inner, true),
                        Err(_) => continue,
                    }
                }
                p => (p, from_peer && self.cfg.security.is_none()),
            };
            if authentic && let Some(s) = session.as_mut() {
                s.last_seen = Instant::now();
                if s.link.confirm() {
                    // The stream starts now, with a picture of its own.
                    s.shared.keyframe_requested.store(true, Ordering::Relaxed);
                    log::debug!("session {:08x}: client confirmed", s.params.session_id);
                }
            }

            match packet {
                Packet::Hello(hello) if self.cfg.security.is_none() => {
                    if !from_peer {
                        if let Some(old) = session.take() {
                            self.replace_session(old, &from.to_string(), &mut out);
                        }
                        let params = self.negotiate(&hello);
                        match self.start_session(params, from, None, None) {
                            Ok((s, _)) => {
                                log_session(&s, &from.to_string());
                                self.set_status(Some(&s), &from.to_string());
                                session = Some(s);
                            }
                            Err(e) => {
                                log::error!("session {:08x}: {e:#}", params.session_id);
                                self.refuse_unavailable(from, &mut out);
                                continue;
                            }
                        }
                    }
                    let Some(s) = session.as_ref() else { continue };
                    let n = self.hello_ack(&s.params, s.codec).encode(&mut out);
                    let _ = self.socket.send_to(&out[..n], from);
                }
                Packet::Handshake(h) if !h.reply => {
                    if handshake_budget.take(from.ip(), Instant::now()) {
                        let log_refusal = refusal_logs.take(from.ip(), Instant::now());
                        self.on_handshake(h.message, from, &mut session, log_refusal, &mut out);
                    }
                }
                Packet::Pair(p) => self.on_pair(p, from, &mut pairing, &mut out),
                Packet::Discover(d) => {
                    if announce_budget.take(from.ip(), Instant::now()) {
                        self.announce(d, from, session.is_some(), &mut out);
                    }
                }
                Packet::ClockPing(ping) => {
                    let pong = ClockPong {
                        seq: ping.seq,
                        client_send_us: ping.client_send_us,
                        host_recv_us: recv_us,
                        host_send_us: now_us(),
                    };
                    let n = pong.encode(&mut out);
                    if self.cfg.security.is_none() {
                        let _ = self.socket.send_to(&out[..n], from);
                    } else if authentic && let Some(s) = session.as_ref() {
                        let _ = s.link.send(&out[..n]);
                    }
                }
                Packet::Feedback(fb) if authentic => {
                    if let Some(s) = session.as_mut()
                        && fb.session_id == s.params.session_id
                    {
                        on_feedback(s, &fb);
                    }
                }
                Packet::Input(h, body) if authentic => {
                    let Some(s) = session.as_mut() else { continue };
                    if h.session_id != s.params.session_id {
                        continue;
                    }
                    s.apply_input(body);
                    let ack = InputAck {
                        session_id: s.params.session_id,
                        seq: s.input_seen.last().unwrap_or(0),
                    };
                    let n = ack.encode(&mut out);
                    let _ = s.link.send(&out[..n]);
                }
                Packet::KeyframeRequest(k) if authentic => {
                    if let Some(s) = session.as_mut()
                        && k.session_id == s.params.session_id
                    {
                        s.client_rtt = Duration::from_micros(u64::from(k.rtt_us));
                        s.shared.want_keyframe(k.last_complete, s.client_rtt);
                    }
                }
                Packet::SelectMonitor(m) if authentic => {
                    if let Some(s) = session.as_ref()
                        && m.session_id == s.params.session_id
                    {
                        s.shared.select_monitor(m.index);
                    }
                }
                Packet::Bye(bye) if authentic => {
                    if let Some(s) = session.take() {
                        if bye.session_id == s.params.session_id {
                            log::info!("session {:08x}: closed by client", s.params.session_id);
                            s.stop();
                            self.set_status(None, "");
                        } else {
                            session = Some(s);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(s) = session.take() {
            say_bye(&s, &mut out);
            s.stop();
        }
        control_stop.store(true, Ordering::Relaxed);
        if let Some(t) = control {
            let _ = t.join();
        }
        if let Some(w) = self.web.take() {
            w.stop.store(true, Ordering::Relaxed);
            let _ = w.thread.join();
        }
        Ok(())
    }
}
