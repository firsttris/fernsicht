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
mod dispatch;
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
use std::time::Duration;

use anyhow::Context;
use crossbeam_channel::{Receiver, bounded};
use fernsicht_core::now_us;
use fernsicht_core::thread::make_current_hot;
use fernsicht_net::socket;
use fernsicht_proto::{MAX_DATAGRAM, Packet};

use session::say_bye;

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
        let mut l = self.new_loop();
        let control_stop = Arc::new(AtomicBool::new(false));
        let control = self.serve_control(&control_stop)?;

        while !stop.load(Ordering::Relaxed) {
            self.end_if_over(&mut l);
            if let Some(w) = &self.web {
                while let Ok(req) = w.requests.try_recv() {
                    self.on_web_request(req, &mut l.session);
                }
            }
            let Some((len, from)) = self.receive(&mut buf) else {
                continue;
            };
            let recv_us = now_us();
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
        if let Some(w) = self.web.take() {
            w.stop.store(true, Ordering::Relaxed);
            let _ = w.thread.join();
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
            self.status.clone(),
            self.cfg.settings.clone(),
            stop.clone(),
        )
        .with_context(|| format!("control socket {}", path.display()))?;
        log::info!("control socket {}", path.display());
        Ok(Some(t))
    }

    /// The next datagram, if one came before the socket's timeout.
    fn receive(&self, buf: &mut [u8]) -> Option<(usize, SocketAddr)> {
        match self.socket.recv_from(buf) {
            Ok(r) => Some(r),
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
