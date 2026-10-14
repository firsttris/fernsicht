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
mod clipboard;
mod config;
pub mod control;
mod dispatch;
mod encode;
mod health;
mod http;
mod input;
pub mod internet;
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
mod sockets;
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
    AudioKind, CaptureKind, DEFAULT_STUN, EncoderKind, HostConfig, HostDescription, HostSettings,
    InputKind, SessionParams,
};
pub use security::{HostSecurity, PAIRING_ATTEMPTS, PAIRING_ATTEMPTS_TOTAL, PAIRING_OPEN_FOR};
pub use shared::HostStats;
#[cfg(feature = "ts")]
pub use web::export_viewer_types;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use crossbeam_channel::{Receiver, bounded};
use fernsicht_core::now_us;
use fernsicht_core::thread::make_current_hot;
use fernsicht_net::socket;
use fernsicht_proto::{MAX_DATAGRAM, Packet};

use session::say_bye;

/// What [`HostAgent::run`] ends with when an encoder call never returned
/// (a GPU fault): the process should go (without exit handlers, which may
/// wait on the same driver), and its service manager starts a fresh one.
#[derive(Debug)]
pub struct EncoderHung;

impl std::fmt::Display for EncoderHung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the encoder stopped responding (a GPU fault); the host has to start afresh")
    }
}

impl std::error::Error for EncoderHung {}

pub struct HostAgent {
    cfg: HostConfig,
    sockets: sockets::Sockets,
    stats: Arc<HostStats>,
    /// The current session, for the control socket.
    status: control::StatusCell,
    /// How the host stands with its rendezvous service, for the control
    /// socket.
    internet: internet::InternetCell,
    /// The thread talking to the rendezvous service, while the host runs.
    signaling: Option<internet::signaling::Signaling>,
    /// The public address the router forwards to the host, if asked to.
    forwarded: internet::forwarding::ForwardedCell,
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
        let sockets =
            sockets::Sockets::bind(&cfg.bind).with_context(|| format!("bind {}", cfg.bind))?;
        // Small and urgent by default (pointer, sound, answers); video
        // marks its own datagrams.
        if let Err(e) = sockets.set_dscp(socket::Dscp::Interactive) {
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
                let public = web::Public {
                    names: cfg.web_public.clone(),
                    stun: cfg.stun.clone(),
                };
                let (thread, addr) = web::serve(
                    addr,
                    cfg.web_root.clone(),
                    sec.clone(),
                    tx,
                    public,
                    stop.clone(),
                )?;
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
            sockets,
            stats: Arc::default(),
            status: Arc::default(),
            internet: Arc::default(),
            signaling: None,
            forwarded: Arc::default(),
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
        self.sockets.local_addr()
    }

    /// Serves sessions until `stop` is set.
    pub fn run(mut self, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
        // Input from the client is applied on this thread.
        make_current_hot("control");
        let mut buf = [0u8; 2048];
        let mut opened = [0u8; MAX_DATAGRAM];
        let mut l = self.new_loop();
        let control_stop = Arc::new(AtomicBool::new(false));
        let control = self.serve_control(&control_stop)?;
        // The rendezvous service, for clients over the internet (idle while
        // none is set).
        // The router's port forwarding, while the setting is on.
        let forwarding = match (&self.cfg.security, self.local_addr()) {
            (Some(_), Ok(local)) => Some(internet::forwarding::spawn(
                self.cfg.settings.clone(),
                local.port(),
                self.internet.clone(),
                self.forwarded.clone(),
                control_stop.clone(),
            )?),
            _ => None,
        };
        if let Some(sec) = &self.cfg.security {
            self.signaling = Some(internet::signaling::spawn(
                self.cfg.settings.clone(),
                sec.identity.clone(),
                self.cfg.stun.clone(),
                self.internet.clone(),
                control_stop.clone(),
            )?);
        }

        while !stop.load(Ordering::Relaxed) && !l.wedged {
            self.end_if_over(&mut l);
            if l.wedged {
                break;
            }
            if let Some(s) = l.session.as_mut()
                && let Some(c) = s.clipboard.as_mut()
            {
                c.tick(&s.link, s.params.session_id, now_us());
            }
            if let Some(w) = &self.web {
                while let Ok(req) = w.requests.try_recv() {
                    self.on_web_request(req, &mut l.session);
                }
            }
            self.internet_tick(&mut l);
            let Some((len, from)) = self.receive(&mut buf) else {
                continue;
            };
            let recv_us = now_us();
            // STUN answers share the port (RFC 7983's first byte).
            if fernsicht_internet::stun::demux(&buf[..len]) == fernsicht_internet::stun::Demux::Stun
            {
                self.on_stun(&mut l, &buf[..len], from);
                continue;
            }
            let outer = match Packet::decode(&buf[..len]) {
                Ok(p) => p,
                Err(e) => {
                    log::debug!("{from}: dropping datagram: {e}");
                    continue;
                }
            };
            if let Some((packet, authentic)) = self.open(&mut l, outer, from, &mut opened) {
                self.on_packet(&mut l, packet, from, authentic, recv_us);
            }
        }
        if let Some(s) = l.session.take() {
            say_bye(&s, &mut l.out);
            s.stop();
        }
        control_stop.store(true, Ordering::Relaxed);
        if let Some(t) = control {
            let _ = t.join();
        }
        if let Some(s) = self.signaling.take() {
            s.join();
        }
        if let Some(t) = forwarding {
            let _ = t.join();
        }
        if let Some(w) = self.web.take() {
            w.stop.store(true, Ordering::Relaxed);
            let _ = w.thread.join();
        }
        if l.wedged {
            return Err(EncoderHung.into());
        }
        Ok(())
    }

    /// The control socket for the app, on a host with keys.
    fn serve_control(&self, stop: &Arc<AtomicBool>) -> anyhow::Result<Option<JoinHandle<()>>> {
        let (Some(path), Some(sec)) = (&self.cfg.control, &self.cfg.security) else {
            return Ok(None);
        };
        let t = control::serve(
            path,
            sec.clone(),
            control::Reports {
                session: self.status.clone(),
                internet: self.internet.clone(),
            },
            self.cfg.settings.clone(),
            stop.clone(),
        )
        .with_context(|| format!("control socket {}", path.display()))?;
        log::info!("control socket {}", path.display());
        Ok(Some(t))
    }

    /// The next datagram, if one came within the control loop's tick.
    fn receive(&self, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
        match self.sockets.recv_from(buf, Duration::from_millis(100)) {
            Ok(r) => r,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                None
            }
            Err(e) => {
                log::warn!("recv: {e}");
                None
            }
        }
    }
}
