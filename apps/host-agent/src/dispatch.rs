//! The control loop's steps: ending a session that is over, telling
//! authentic packets from the rest, and answering each packet.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use fernsicht_core::now_us;
use fernsicht_proto::{ClockPing, ClockPong, Hello, InputAck, InputHeader, MAX_DATAGRAM, Packet};

use crate::HostAgent;
use crate::health::{self, End};
use crate::limits::PerSource;
use crate::security::PairingExchange;
use crate::session::{Session, log_session, on_feedback};

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

/// The control loop's state between datagrams.
pub(crate) struct Loop {
    pub(crate) session: Option<Session>,
    /// Connections over the internet being set up.
    pub(crate) punching: crate::internet::punching::Punching,
    pairing: Option<PairingExchange>,
    announce_budget: PerSource,
    handshake_budget: PerSource,
    refusal_logs: PerSource,
    /// How often devices were unpaired, when last looked.
    revocations_seen: u64,
    /// Answers are encoded here.
    pub(crate) out: [u8; MAX_DATAGRAM],
    /// An encoder hung: the GPU needs a fresh process, the host ends.
    pub(crate) wedged: bool,
}

impl HostAgent {
    pub(crate) fn new_loop(&self) -> Loop {
        let now = Instant::now();
        Loop {
            session: None,
            punching: Default::default(),
            pairing: None,
            announce_budget: PerSource::new(ANNOUNCE_PER_SOURCE, ANNOUNCE_TOTAL, now),
            handshake_budget: PerSource::new(HANDSHAKES_PER_SOURCE, HANDSHAKES_TOTAL, now),
            refusal_logs: PerSource::new(REFUSAL_LOGS_PER_SOURCE, REFUSAL_LOGS_TOTAL, now),
            revocations_seen: self.cfg.security.as_ref().map_or(0, |s| s.revocations()),
            out: [0; MAX_DATAGRAM],
            wedged: false,
        }
    }

    /// Ends the session when the user ended it here, its device was
    /// unpaired, its threads stopped or its client went quiet.
    pub(crate) fn end_if_over(&self, l: &mut Loop) {
        let revoked = self.cfg.security.as_ref().and_then(|sec| {
            let now = sec.revocations();
            (now != l.revocations_seen).then(|| {
                l.revocations_seen = now;
                sec
            })
        });
        // Taken every time: a request while no session runs must not end
        // the next one.
        let ended_here = self
            .cfg
            .security
            .as_ref()
            .is_some_and(|sec| sec.take_end_request());
        let end = l.session.as_ref().and_then(|s| {
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
                (s.last_seen.elapsed(), self.cfg.client_timeout),
                health::busy_for(s.shared.encoding_since.load(Ordering::Acquire), now_us()),
            )
        });
        if let Some(why) = end
            && let Some(s) = l.session.take()
        {
            l.wedged |= why == End::EncoderHung;
            self.end_session(s, why, &mut l.out);
        }
    }

    /// The packet inside `outer` and whether it is authentic: the sealed
    /// packets of the current session (opened into `opened`) are the only
    /// authentic ones of a secure session. An authentic packet keeps the
    /// session alive; the first one starts its stream.
    pub(crate) fn open<'a>(
        &self,
        l: &mut Loop,
        outer: Packet<'a>,
        from: SocketAddr,
        opened: &'a mut [u8],
    ) -> Option<(Packet<'a>, bool)> {
        let from_peer = l.session.as_ref().is_some_and(|s| s.peer == from);
        let (packet, authentic) = match outer {
            Packet::Sealed(h, sealed) => {
                let s = l.session.as_ref()?;
                if !from_peer || h.session_id != s.params.session_id {
                    return None;
                }
                let n = match s.link.open(h.counter, sealed, opened)? {
                    Ok(n) => n,
                    Err(e) => {
                        log::debug!("{from}: dropping sealed packet: {e}");
                        return None;
                    }
                };
                let opened: &'a [u8] = opened;
                (Packet::decode(&opened[..n]).ok()?, true)
            }
            p => (p, from_peer && self.cfg.security.is_none()),
        };
        if authentic && let Some(s) = l.session.as_mut() {
            s.last_seen = Instant::now();
            if s.link.confirm() {
                // The stream starts now, with a picture of its own.
                s.shared.keyframe_requested.store(true, Ordering::Relaxed);
                log::debug!("session {:08x}: client confirmed", s.params.session_id);
                self.send_rendezvous(s);
            }
        }
        Some((packet, authentic))
    }

    /// Answers one packet; `recv_us` is when it arrived.
    pub(crate) fn on_packet(
        &self,
        l: &mut Loop,
        packet: Packet<'_>,
        from: SocketAddr,
        authentic: bool,
        recv_us: u64,
    ) {
        let ours = |s: &Session, id: u32| s.params.session_id == id;
        match packet {
            Packet::Hello(hello) if self.cfg.security.is_none() => self.on_hello(l, &hello, from),
            Packet::Handshake(h) if !h.reply => {
                if l.handshake_budget.take(from.ip(), Instant::now()) {
                    let log_refusal = l.refusal_logs.take(from.ip(), Instant::now());
                    let relayed = l.punching.is_relayed(from, Instant::now());
                    self.on_handshake(
                        h.message,
                        from,
                        &mut l.session,
                        (log_refusal, relayed),
                        &mut l.out,
                    );
                }
            }
            Packet::Punch(p) => self.on_punch(l, p, from),
            Packet::Pair(p) => self.on_pair(p, from, &mut l.pairing, &mut l.out),
            Packet::Discover(d) => {
                if l.announce_budget.take(from.ip(), Instant::now()) {
                    self.announce(d, from, l.session.is_some(), &mut l.out);
                }
            }
            Packet::ClockPing(ping) => self.on_clock_ping(l, &ping, from, authentic, recv_us),
            Packet::Feedback(fb) if authentic => {
                if let Some(s) = l.session.as_mut()
                    && ours(s, fb.session_id)
                {
                    on_feedback(s, &fb);
                }
            }
            Packet::Input(h, body) if authentic => on_input(l, &h, body),
            Packet::Clipboard(c, data) if authentic => {
                if let Some(s) = l.session.as_mut()
                    && ours(s, c.session_id)
                    && let Some(clip) = s.clipboard.as_mut()
                {
                    clip.piece(&s.link, &c, data);
                }
            }
            Packet::ClipboardAck(a) if authentic => {
                if let Some(s) = l.session.as_mut()
                    && ours(s, a.session_id)
                    && let Some(clip) = s.clipboard.as_mut()
                {
                    clip.acked(&a);
                }
            }
            Packet::Nack(n) if authentic => {
                if let Some(s) = l.session.as_ref()
                    && ours(s, n.session_id)
                {
                    // Full: the sender is behind and the answer late.
                    let _ = s.shared.nacks.0.try_send(n);
                }
            }
            Packet::KeyframeRequest(k) if authentic => {
                if let Some(s) = l.session.as_mut()
                    && ours(s, k.session_id)
                {
                    s.client_rtt = Duration::from_micros(u64::from(k.rtt_us));
                    s.shared.want_keyframe(k.last_complete, s.client_rtt);
                }
            }
            Packet::SelectMonitor(m) if authentic => {
                if let Some(s) = l.session.as_ref()
                    && ours(s, m.session_id)
                {
                    s.shared.select_monitor(m.index);
                }
            }
            Packet::Bye(bye) if authentic => {
                if let Some(s) = l.session.take_if(|s| ours(s, bye.session_id)) {
                    log::info!("session {:08x}: closed by client", s.params.session_id);
                    s.stop();
                    self.set_status(None, "");
                }
            }
            _ => {}
        }
    }

    /// A plain session (the library's insecure mode, tests): a hello from
    /// a new address replaces the session; every hello gets the HelloAck.
    fn on_hello(&self, l: &mut Loop, hello: &Hello, from: SocketAddr) {
        if l.session.as_ref().is_none_or(|s| s.peer != from) {
            if let Some(old) = l.session.take() {
                self.replace_session(old, &from.to_string(), &mut l.out);
            }
            let params = self.negotiate(hello);
            match self.start_session(params, from, None, None) {
                Ok((s, _)) => {
                    log_session(&s, &from.to_string());
                    self.set_status(Some(&s), &from.to_string());
                    l.session = Some(s);
                }
                Err(e) => {
                    log::error!("session {:08x}: {e:#}", params.session_id);
                    self.refuse_unavailable(from, &mut l.out);
                    return;
                }
            }
        }
        let Some(s) = l.session.as_ref() else { return };
        let n = self.hello_ack(&s.params, s.codec).encode(&mut l.out);
        let _ = self.sockets.send_to(&l.out[..n], from);
    }

    /// The pong for the client's clock sync: plain in the insecure mode,
    /// else only to an authentic ping, sealed.
    fn on_clock_ping(
        &self,
        l: &mut Loop,
        ping: &ClockPing,
        from: SocketAddr,
        authentic: bool,
        recv_us: u64,
    ) {
        let pong = ClockPong {
            seq: ping.seq,
            client_send_us: ping.client_send_us,
            host_recv_us: recv_us,
            host_send_us: now_us(),
        };
        let n = pong.encode(&mut l.out);
        if self.cfg.security.is_none() {
            let _ = self.sockets.send_to(&l.out[..n], from);
        } else if authentic && let Some(s) = l.session.as_ref() {
            let _ = s.link.send(&l.out[..n]);
        }
    }
}

/// Applies the client's input and acks the newest event seen.
fn on_input(l: &mut Loop, h: &InputHeader, body: &[u8]) {
    let Some(s) = l.session.as_mut() else { return };
    if h.session_id != s.params.session_id {
        return;
    }
    s.apply_input(body);
    let ack = InputAck {
        session_id: s.params.session_id,
        seq: s.input_seen.last().unwrap_or(0),
    };
    let n = ack.encode(&mut l.out);
    let _ = s.link.send(&l.out[..n]);
}
