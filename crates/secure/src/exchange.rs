//! A single Noise IK exchange between paired devices, carried by someone
//! else: the rendezvous service passes a client's sealed offer to its host
//! and the host's sealed answer back (see `docs/internet.md`).
//!
//! The same pattern as a session's handshake, but bound to its purpose by
//! a prologue (an offer cannot be replayed as a session handshake, nor the
//! other way round), with larger payloads, and no transport after it: the
//! two messages are all there is.

use crate::session::{PARAMS, SessionError};
use crate::{Identity, PublicKey};

/// Largest exchange message (Noise's own limit is 65,535 bytes).
pub const MAX_MESSAGE: usize = 8 * 1024;

fn err(what: &str) -> impl Fn(snow::Error) -> SessionError + '_ {
    move |e| SessionError(format!("{what}: {e}"))
}

fn builder<'a>(prologue: &'a [u8]) -> snow::Builder<'a> {
    snow::Builder::new(PARAMS.parse().expect("valid Noise params"))
        .prologue(prologue)
        .expect("a prologue is set once")
}

/// The client's side, waiting for the answer.
pub struct Offered {
    hs: snow::HandshakeState,
}

/// Seals `payload` for `host` only: the first message.
pub fn offer(
    identity: &Identity,
    host: &PublicKey,
    prologue: &[u8],
    payload: &[u8],
) -> Result<(Offered, Vec<u8>), SessionError> {
    let mut hs = builder(prologue)
        .local_private_key(identity.secret())
        .map_err(err("local key"))?
        .remote_public_key(&host.0)
        .map_err(err("host key"))?
        .build_initiator()
        .map_err(err("offer"))?;
    let mut msg = vec![0u8; MAX_MESSAGE];
    let n = hs.write_message(payload, &mut msg).map_err(err("offer"))?;
    msg.truncate(n);
    Ok((Offered { hs }, msg))
}

impl Offered {
    /// The host's answer, opened; fails unless it comes from the host.
    pub fn open_answer(mut self, msg: &[u8]) -> Result<Vec<u8>, SessionError> {
        let mut payload = vec![0u8; MAX_MESSAGE];
        let n = self
            .hs
            .read_message(msg, &mut payload)
            .map_err(err("answer"))?;
        payload.truncate(n);
        Ok(payload)
    }
}

/// An offer the host opened: who sent it and what it says.
pub struct Opened {
    hs: snow::HandshakeState,
    /// The client's long-term key: check it is paired before answering.
    pub client: PublicKey,
    pub payload: Vec<u8>,
}

/// Opens an offer sealed for `identity`.
pub fn open_offer(
    identity: &Identity,
    prologue: &[u8],
    msg: &[u8],
) -> Result<Opened, SessionError> {
    let mut hs = builder(prologue)
        .local_private_key(identity.secret())
        .map_err(err("local key"))?
        .build_responder()
        .map_err(err("offer"))?;
    let mut payload = vec![0u8; MAX_MESSAGE];
    let n = hs.read_message(msg, &mut payload).map_err(err("offer"))?;
    payload.truncate(n);
    let remote = hs
        .get_remote_static()
        .ok_or_else(|| SessionError("no client key".into()))?;
    let mut client = [0u8; 32];
    client.copy_from_slice(remote);
    Ok(Opened {
        hs,
        client: PublicKey(client),
        payload,
    })
}

impl Opened {
    /// Seals `payload` for the client that sent the offer.
    pub fn answer(mut self, payload: &[u8]) -> Result<Vec<u8>, SessionError> {
        let mut msg = vec![0u8; MAX_MESSAGE];
        let n = self
            .hs
            .write_message(payload, &mut msg)
            .map_err(err("answer"))?;
        msg.truncate(n);
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &[u8] = b"fernsicht test v1";

    #[test]
    fn offer_and_answer_reach_only_each_other() {
        let host = Identity::generate();
        let client = Identity::generate();
        let (offered, msg1) = offer(&client, &host.public, P, b"candidates").unwrap();
        let opened = open_offer(&host, P, &msg1).unwrap();
        assert_eq!(opened.client, client.public);
        assert_eq!(opened.payload, b"candidates");
        let msg2 = opened.answer(&[7u8; 2000]).unwrap();
        assert_eq!(offered.open_answer(&msg2).unwrap(), vec![7u8; 2000]);
        // Another host cannot open it.
        assert!(open_offer(&Identity::generate(), P, &msg1).is_err());
    }

    #[test]
    fn the_prologue_binds_the_purpose() {
        let host = Identity::generate();
        let client = Identity::generate();
        let (_, msg1) = offer(&client, &host.public, P, b"x").unwrap();
        assert!(open_offer(&host, b"other purpose", &msg1).is_err());
        // Nor is it a session handshake.
        assert!(crate::session::Responder::read(&host, &msg1).is_err());
    }

    #[test]
    fn a_forged_answer_is_refused() {
        let host = Identity::generate();
        let client = Identity::generate();
        let (offered, msg1) = offer(&client, &host.public, P, b"x").unwrap();
        let mut msg2 = open_offer(&host, P, &msg1).unwrap().answer(b"ok").unwrap();
        msg2[40] ^= 1;
        assert!(offered.open_answer(&msg2).is_err());
        assert!(open_offer(&host, P, b"garbage").is_err());
    }
}
