//! The control loop's answers: handshakes, pairing, discovery, the web
//! viewer's requests, and ending sessions.

use std::net::SocketAddr;

use fernsicht_proto::{
    Announce, Discover, Handshake, Hello, MAX_ANNOUNCE_INFO, MAX_ANNOUNCE_NAME, MAX_DATAGRAM, Pair,
    RejectReason, clip,
};
use fernsicht_secure::pairing::HostPairing;
use fernsicht_secure::session::Responder;

use crate::HostAgent;
use crate::config::SessionParams;
use crate::health::End;
use crate::security::PairingExchange;
use crate::session::{Session, log_session, new_session_id, say_bye};

impl HostAgent {
    /// Ends a session the control loop found over (`why`); a client whose
    /// stream stopped is told, so it doesn't watch a frozen picture.
    pub(crate) fn end_session(&self, s: Session, why: End, out: &mut [u8]) {
        log::info!("session {:08x}: {}", s.params.session_id, why.describe());
        if why.tells_the_client() {
            say_bye(&s, out);
        }
        s.stop();
        self.set_status(None, "");
    }

    /// A browser asks for a session with a PIN: on a right PIN it replaces
    /// the running session.
    pub(crate) fn on_web_request(
        &self,
        req: crate::web::WebRequest,
        session: &mut Option<Session>,
    ) {
        let Some(sec) = &self.cfg.security else {
            let _ = req.reply.send(Err(crate::web::WebError::Failed));
            return;
        };
        if let Err(e) = crate::web::check_pin(sec, &req.pin, req.browser.ip()) {
            log::info!("web: {} turned away ({e:?})", req.browser.ip());
            let _ = req.reply.send(Err(e));
            return;
        }
        let (setup, answer) = match crate::web::accept(req.offer, req.local_ip) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("web: {}: {e:#}", req.browser.ip());
                let _ = req.reply.send(Err(crate::web::WebError::Failed));
                return;
            }
        };
        if let Some(old) = session.take() {
            let mut out = [0u8; MAX_DATAGRAM];
            self.replace_session(old, &format!("a browser at {}", req.browser.ip()), &mut out);
        }
        let params = SessionParams {
            session_id: new_session_id(),
            // The screen's own size (fitted into the limits), 60 fps.
            width: 0,
            height: 0,
            fps: 60.min(self.cfg.max_fps),
            bitrate_kbps: 0,
            // What the browser offered: H.264, and AV1 from Chrome and
            // Firefox (they decode it on the CPU if need be).
            codecs: setup.browser_codecs(),
        };
        let who = format!("Browser {}", req.browser.ip());
        match self.start_session(params, req.browser, None, Some(setup)) {
            Ok((s, _)) => {
                let accepted = crate::web::WebAccepted {
                    answer,
                    params: s.params,
                };
                // The browser's request timed out meanwhile: it never gets
                // the answer and never connects.
                if req.reply.send(Ok(accepted)).is_err() {
                    log::info!(
                        "session {:08x}: the browser stopped waiting",
                        s.params.session_id
                    );
                    s.stop();
                    return;
                }
                log_session(&s, &who);
                self.set_status(Some(&s), &who);
                *session = Some(s);
            }
            Err(e) => {
                log::error!("session {:08x}: {e:#}", params.session_id);
                let _ = req.reply.send(Err(crate::web::WebError::Failed));
            }
        }
    }

    /// What the control socket reports; `None` when no session runs.
    pub(crate) fn set_status(&self, s: Option<&Session>, client: &str) {
        let status = s.map(|s| crate::control::SessionStatus {
            client: client.to_owned(),
            address: s.peer.to_string(),
            width: s.params.width,
            height: s.params.height,
            fps: s.params.fps,
            encrypted: s.link.encrypted(),
            since: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        });
        if let Ok(mut cell) = self.status.lock() {
            *cell = status;
        }
    }

    /// A client's first handshake message: a paired client gets a session
    /// (encrypted from here on), anyone else a "not paired" hint.
    pub(crate) fn on_handshake(
        &self,
        msg: &[u8],
        from: SocketAddr,
        session: &mut Option<Session>,
        // Whether a refusal may be logged (rate-limited per address).
        log_refusal: bool,
        out: &mut [u8],
    ) {
        let Some(sec) = &self.cfg.security else {
            return;
        };
        // A retransmission of the handshake that started this session.
        if let Some(s) = session.as_ref()
            && s.peer == from
            && let Some((first, reply)) = &s.handshake
            && first.as_slice() == msg
        {
            let n = Handshake {
                reply: true,
                message: reply,
            }
            .encode(out);
            let _ = self.socket.send_to(&out[..n], from);
            return;
        }
        let responder = match Responder::read(&sec.identity, msg) {
            Ok(r) => r,
            Err(e) => {
                log::debug!("{from}: handshake: {e}");
                return;
            }
        };
        let Some(client) = sec.is_paired(&responder.client) else {
            if log_refusal {
                log::warn!(
                    "{from}: refused an unpaired device ({}); pair it with --pair",
                    responder.client.fingerprint()
                );
            }
            let n = RejectReason::NotPaired.encode(out);
            let _ = self.socket.send_to(&out[..n], from);
            return;
        };
        let Some((hello, sent_ms)) = Hello::decode_with_time(&responder.payload) else {
            return;
        };
        if !sec.handshake_is_new(&client.key, sent_ms) {
            log::warn!("{from}: replayed handshake of {} dropped", client.name);
            return;
        }
        // The old session goes first: screen and encoder may not be had
        // twice.
        if let Some(old) = session.take() {
            self.replace_session(old, &from.to_string(), out);
        }
        let params = self.negotiate(&hello);
        match self.start_session(params, from, Some(responder), None) {
            Ok((mut s, Some(reply))) => {
                // Noted only now: had the start failed, a later try with
                // the same handshake would count as a replay.
                sec.note_handshake(client.key, sent_ms);
                s.client = Some(client.key);
                log_session(&s, &format!("{} ({from})", client.name));
                self.set_status(Some(&s), &client.name);
                let n = Handshake {
                    reply: true,
                    message: &reply,
                }
                .encode(out);
                let _ = self.socket.send_to(&out[..n], from);
                s.handshake = Some((msg.to_vec(), reply));
                *session = Some(s);
            }
            Ok((s, None)) => s.stop(),
            Err(e) => {
                log::error!("session {:08x}: {e:#}", params.session_id);
                self.refuse_unavailable(from, out);
            }
        }
    }

    /// Ends the running session for a new client: the old client is told
    /// (instead of watching a frozen picture until its timeout), and the
    /// status shows no session until the new one runs.
    pub(crate) fn replace_session(&self, old: Session, by: &str, out: &mut [u8]) {
        log::info!("session {:08x}: replaced by {by}", old.params.session_id);
        say_bye(&old, out);
        old.stop();
        self.set_status(None, "");
    }

    /// Tells a client its session could not start, so it stops asking (each
    /// try would open screen and encoder again) and says why.
    pub(crate) fn refuse_unavailable(&self, to: SocketAddr, out: &mut [u8]) {
        let n = RejectReason::Unavailable.encode(out);
        let _ = self.socket.send_to(&out[..n], to);
    }

    /// Answers "which hosts are there?" with name, key, OS and GPU. Only
    /// a host with a key answers (the library's insecure test mode not).
    pub(crate) fn announce(&self, d: Discover, from: SocketAddr, busy: bool, out: &mut [u8]) {
        let Some(sec) = self.cfg.security.as_ref() else {
            return;
        };
        let name = match clip(sec.name(), MAX_ANNOUNCE_NAME) {
            "" => "host",
            n => n,
        };
        let a = Announce {
            nonce: d.nonce,
            key: sec.public_key().0,
            pairing: sec.pairing_remaining().is_some(),
            busy,
            name,
            os: clip(&self.cfg.description.os, MAX_ANNOUNCE_INFO),
            gpu: clip(&self.cfg.description.gpu, MAX_ANNOUNCE_INFO),
        };
        let n = a.encode(out);
        let _ = self.socket.send_to(&out[..n], from);
    }

    /// Pairing messages (only while pairing mode is open).
    pub(crate) fn on_pair(
        &self,
        p: Pair<'_>,
        from: SocketAddr,
        ex: &mut Option<PairingExchange>,
        out: &mut [u8],
    ) {
        let Some(sec) = &self.cfg.security else {
            return;
        };
        let send = |step: u8, message: &[u8], out: &mut [u8]| {
            let n = Pair { step, message }.encode(out);
            let _ = self.socket.send_to(&out[..n], from);
        };
        match p.step {
            1 => {
                if let Some(e) = ex.as_ref()
                    && e.from == from
                    && e.msg1 == p.message
                {
                    send(2, &e.msg2, out);
                    return;
                }
                // A message that cannot be one costs no attempt.
                if !fernsicht_secure::pairing::message1_is_well_formed(p.message) {
                    log::debug!("{from}: pairing: malformed message 1");
                    return;
                }
                let (pin, generation) = match sec.pairing_attempt(from.ip()) {
                    Ok(pin) => pin,
                    Err(reason) => {
                        let n = reason.encode(out);
                        let _ = self.socket.send_to(&out[..n], from);
                        return;
                    }
                };
                match HostPairing::respond(&pin, &sec.identity, &sec.name, p.message) {
                    Ok((state, msg2)) => {
                        send(2, &msg2, out);
                        *ex = Some(PairingExchange {
                            generation,
                            from,
                            msg1: p.message.to_vec(),
                            msg2,
                            state: Some(state),
                            done: None,
                        });
                    }
                    Err(e) => log::debug!("{from}: pairing: {e}"),
                }
            }
            3 => {
                let Some(e) = ex.as_mut().filter(|e| e.from == from) else {
                    return;
                };
                if let Some((msg3, msg4)) = &e.done {
                    if msg3.as_slice() == p.message {
                        send(4, msg4, out);
                    }
                    return;
                }
                // Its window still open: not closed meanwhile, and its PIN
                // not used by a browser (which sends it in clear).
                if sec.pairing_generation() != Some(e.generation) {
                    log::warn!("{from}: pairing refused: pairing closed meanwhile");
                    *ex = None;
                    let n = RejectReason::PairingClosed.encode(out);
                    let _ = self.socket.send_to(&out[..n], from);
                    return;
                }
                let Some(state) = e.state.take() else { return };
                match state.finish(p.message) {
                    Ok((peer, msg4)) => {
                        log::info!(
                            "paired with {} ({}, key {})",
                            peer.name,
                            from.ip(),
                            peer.key.fingerprint()
                        );
                        sec.add(peer);
                        sec.close_pairing();
                        send(4, &msg4, out);
                        e.done = Some((p.message.to_vec(), msg4));
                    }
                    Err(err) => log::warn!("pairing with {from} failed: {err}"),
                }
            }
            _ => {}
        }
    }
}
