//! Connections over the internet (`docs/internet.md`): a thread keeps the
//! host registered at its rendezvous service ([`signaling`]); the offers
//! it passes on are answered in the control loop, which owns the socket
//! that STUN and the hole punching must use ([`punching`]).

pub(crate) mod forwarding;
pub(crate) mod punching;
pub(crate) mod signaling;

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// How the host stands with its rendezvous service, for the app.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct InternetStatus {
    /// The service's address, if one is set.
    pub url: Option<String>,
    pub state: InternetState,
    /// The service hands out relay (TURN) credentials.
    pub relay: bool,
    /// Why the last attempt failed.
    pub error: Option<String>,
    /// The port the router forwards, if asked to.
    #[serde(default)]
    pub forwarding: Forwarding,
}

/// The port the router forwards to the host (`--port-mapping`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Forwarding {
    /// The host asks its router to forward its port.
    pub on: bool,
    /// The public address the router forwards, once it does.
    pub external: Option<String>,
    /// Why the router does not.
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum InternetState {
    /// No service set: LAN and VPN only.
    #[default]
    Off,
    Connecting,
    /// Registered: reachable over the internet.
    Registered,
    /// The service could not be reached or refused; tried again.
    Failed,
}

/// Shared between the signaling thread (writes) and the control socket.
pub type InternetCell = Arc<Mutex<InternetStatus>>;

use std::net::SocketAddr;
use std::time::Instant;

use fernsicht_internet::candidates::{Candidate, host_candidates, worth_offering};
use fernsicht_proto::{MAX_DATAGRAM, Punch, Rendezvous};

use crate::HostAgent;
use crate::dispatch::Loop;
use crate::session::Session;
use punching::Action;
use signaling::Reply;

impl HostAgent {
    /// The host's own candidates: its addresses on its port (IPv6 ones
    /// only where it listens on IPv6).
    fn local_candidates(&self) -> Vec<Candidate> {
        let Ok(local) = self.sockets.local_addr() else {
            return Vec::new();
        };
        let v6 = self.sockets.has_v6();
        let forwarded = *self.forwarded.lock().unwrap_or_else(|e| e.into_inner());
        host_candidates(local.port())
            .into_iter()
            .filter(|c| worth_offering(c.addr.ip()) && (c.addr.is_ipv4() || v6))
            .chain(forwarded.map(|addr| Candidate {
                kind: fernsicht_internet::candidates::Kind::Mapped,
                addr,
            }))
            .collect()
    }

    /// Carries out what the punching asks for.
    fn act(&self, actions: Vec<Action>, replies: &crossbeam_channel::Sender<Reply>) {
        for a in actions {
            match a {
                Action::Send { to, data } => {
                    let _ = self.sockets.send_to(&data, to);
                }
                Action::Reply(r) => {
                    // Full: the signaling thread is gone or behind; the
                    // client times out and tries again.
                    let _ = replies.try_send(r);
                }
            }
        }
    }

    /// Offers the signaling thread passed on, and what is due.
    pub(crate) fn internet_tick(&self, l: &mut Loop) {
        let (Some(sig), Some(sec)) = (&self.signaling, &self.cfg.security) else {
            return;
        };
        let now = Instant::now();
        while let Ok(offer) = sig.offers.try_recv() {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            let actions = l.punching.offer(
                offer,
                &sec.identity,
                |k| sec.is_paired(k).is_some(),
                self.local_candidates(),
                (now, now_ms),
            );
            self.act(actions, &sig.replies);
        }
        let actions = l.punching.tick(now);
        self.act(actions, &sig.replies);
    }

    /// A STUN answer on the host's socket.
    pub(crate) fn on_stun(&self, l: &mut Loop, datagram: &[u8], from: SocketAddr) {
        if let Some(sig) = &self.signaling {
            let actions = l.punching.stun_answer(datagram, from, Instant::now());
            self.act(actions, &sig.replies);
        }
    }

    /// A client's probe.
    pub(crate) fn on_punch(&self, l: &Loop, p: Punch, from: SocketAddr) {
        if let Some(sig) = &self.signaling {
            self.act(l.punching.punch(p, from), &sig.replies);
        }
    }

    /// Tells a client where the host can be found over the internet, once
    /// its session is confirmed (sealed: nobody else can point it
    /// elsewhere). An empty address: none set.
    pub(crate) fn send_rendezvous(&self, s: &Session) {
        if s.web || self.cfg.security.is_none() {
            return;
        }
        let url = self.cfg.settings.rendezvous().unwrap_or_default();
        if url.len() > fernsicht_proto::MAX_RENDEZVOUS_URL {
            return;
        }
        let mut out = [0u8; MAX_DATAGRAM];
        let n = Rendezvous {
            session_id: s.params.session_id,
            url: &url,
        }
        .encode(&mut out);
        let _ = s.link.send(&out[..n]);
    }
}
