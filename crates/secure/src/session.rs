//! A session between paired devices: Noise IK handshake, then sealed
//! packets.
//!
//! IK: the client knows the host's key from pairing and sends its own
//! encrypted in the first message, so the host can check it against its
//! paired list before answering. One round trip; both sides end up with
//! fresh keys (forward secrecy). The first message's payload is
//! encrypted too, the second's as well.
//!
//! Packets carry an explicit 64-bit counter (UDP loses and reorders, so
//! the nonce cannot be implicit) checked by a [`ReplayWindow`].

use crate::replay::ReplayWindow;
use crate::{Identity, PublicKey};

/// Noise protocol name.
pub const PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// Bytes a sealed packet is longer than its plaintext (Poly1305 tag).
pub const TAG_LEN: usize = 16;
/// Largest handshake message.
pub const MAX_HANDSHAKE: usize = 1024;

#[derive(Debug)]
pub struct SessionError(pub String);

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SessionError {}

fn err(what: &str) -> impl Fn(snow::Error) -> SessionError + '_ {
    move |e| SessionError(format!("{what}: {e}"))
}

fn builder() -> snow::Builder<'static> {
    snow::Builder::new(PARAMS.parse().expect("valid Noise params"))
}

/// The client's side of the handshake.
pub struct Initiator {
    hs: snow::HandshakeState,
}

impl Initiator {
    /// Message 1 to `host`, carrying `payload` (encrypted).
    pub fn start(
        identity: &Identity,
        host: &PublicKey,
        payload: &[u8],
    ) -> Result<(Self, Vec<u8>), SessionError> {
        let mut hs = builder()
            .local_private_key(identity.secret())
            .map_err(err("local key"))?
            .remote_public_key(&host.0)
            .map_err(err("host key"))?
            .build_initiator()
            .map_err(err("handshake"))?;
        let mut msg = vec![0u8; MAX_HANDSHAKE];
        let n = hs
            .write_message(payload, &mut msg)
            .map_err(err("message 1"))?;
        msg.truncate(n);
        Ok((Self { hs }, msg))
    }

    /// Takes message 2; returns the transport and the host's payload.
    /// Fails if the answer is not from the expected host.
    pub fn finish(mut self, msg2: &[u8]) -> Result<(Transport, Vec<u8>), SessionError> {
        let mut payload = vec![0u8; MAX_HANDSHAKE];
        let n = self
            .hs
            .read_message(msg2, &mut payload)
            .map_err(err("message 2"))?;
        payload.truncate(n);
        Ok((Transport::new(self.hs)?, payload))
    }
}

/// The host's side, after reading message 1.
pub struct Responder {
    hs: snow::HandshakeState,
    /// The client's long-term key: check it is paired before replying.
    pub client: PublicKey,
    /// What the client sent along.
    pub payload: Vec<u8>,
}

impl Responder {
    pub fn read(identity: &Identity, msg1: &[u8]) -> Result<Self, SessionError> {
        let mut hs = builder()
            .local_private_key(identity.secret())
            .map_err(err("local key"))?
            .build_responder()
            .map_err(err("handshake"))?;
        let mut payload = vec![0u8; MAX_HANDSHAKE];
        let n = hs
            .read_message(msg1, &mut payload)
            .map_err(err("message 1"))?;
        payload.truncate(n);
        let remote = hs
            .get_remote_static()
            .ok_or_else(|| SessionError("no client key".into()))?;
        let mut client = [0u8; 32];
        client.copy_from_slice(remote);
        Ok(Self {
            hs,
            client: PublicKey(client),
            payload,
        })
    }

    /// Message 2 carrying `payload`; returns the transport.
    pub fn reply(mut self, payload: &[u8]) -> Result<(Transport, Vec<u8>), SessionError> {
        let mut msg = vec![0u8; MAX_HANDSHAKE];
        let n = self
            .hs
            .write_message(payload, &mut msg)
            .map_err(err("message 2"))?;
        msg.truncate(n);
        Ok((Transport::new(self.hs)?, msg))
    }
}

/// Sealing and opening packets of one session. Shared by all threads of
/// a session (`&self`): sealing takes the next counter atomically, opening
/// locks the replay window.
pub struct Transport {
    state: snow::StatelessTransportState,
    next: std::sync::atomic::AtomicU64,
    replay: std::sync::Mutex<ReplayWindow>,
}

impl Transport {
    fn new(hs: snow::HandshakeState) -> Result<Self, SessionError> {
        Ok(Self {
            state: hs
                .into_stateless_transport_mode()
                .map_err(err("transport"))?,
            next: std::sync::atomic::AtomicU64::new(0),
            replay: std::sync::Mutex::new(ReplayWindow::default()),
        })
    }

    /// Seals `plain` into `out` (at least `plain.len() + TAG_LEN`); returns
    /// the counter to send along and the sealed length.
    pub fn seal(&self, plain: &[u8], out: &mut [u8]) -> Result<(u64, usize), SessionError> {
        let counter = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let n = self
            .state
            .write_message(counter, plain, out)
            .map_err(err("seal"))?;
        Ok((counter, n))
    }

    /// Opens a sealed packet into `out`; rejects forgeries and replays.
    pub fn open(&self, counter: u64, sealed: &[u8], out: &mut [u8]) -> Result<usize, SessionError> {
        let mut replay = self.replay.lock().unwrap_or_else(|e| e.into_inner());
        if !replay.check(counter) {
            return Err(SessionError("replayed or too old".into()));
        }
        let n = self
            .state
            .read_message(counter, sealed, out)
            .map_err(err("open"))?;
        replay.commit(counter);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connect() -> (Transport, Transport) {
        let (cid, hid) = (Identity::generate(), Identity::generate());
        let (init, m1) = Initiator::start(&cid, &hid.public, b"hello").unwrap();
        let resp = Responder::read(&hid, &m1).unwrap();
        assert_eq!(resp.client, cid.public);
        assert_eq!(resp.payload, b"hello");
        let (host, m2) = resp.reply(b"ack").unwrap();
        let (client, payload) = init.finish(&m2).unwrap();
        assert_eq!(payload, b"ack");
        (client, host)
    }

    #[test]
    fn both_ways_after_the_handshake() {
        let (c, h) = connect();
        let mut sealed = [0u8; 1500];
        let mut plain = [0u8; 1500];
        let (n0, len) = c.seal(b"input", &mut sealed).unwrap();
        assert_eq!(len, 5 + TAG_LEN);
        assert_eq!(h.open(n0, &sealed[..len], &mut plain).unwrap(), 5);
        assert_eq!(&plain[..5], b"input");
        let (n1, len) = h.seal(&[7u8; 1300], &mut sealed).unwrap();
        assert_eq!(c.open(n1, &sealed[..len], &mut plain).unwrap(), 1300);
    }

    #[test]
    fn replays_forgeries_and_reordering() {
        let (c, h) = connect();
        let mut buf = [0u8; 64];
        let mut out = [0u8; 64];
        let packets: Vec<(u64, Vec<u8>)> = (0..3)
            .map(|i| {
                let (n, len) = c.seal(&[i], &mut buf).unwrap();
                (n, buf[..len].to_vec())
            })
            .collect();
        // Reordered: fine.
        h.open(packets[2].0, &packets[2].1, &mut out).unwrap();
        h.open(packets[0].0, &packets[0].1, &mut out).unwrap();
        // Replayed: rejected.
        assert!(h.open(packets[0].0, &packets[0].1, &mut out).is_err());
        // Tampered, or with another counter: rejected, and the real one
        // still opens afterwards (forgeries do not burn counters).
        let mut bad = packets[1].1.clone();
        bad[0] ^= 1;
        assert!(h.open(packets[1].0, &bad, &mut out).is_err());
        assert!(h.open(99, &packets[1].1, &mut out).is_err());
        h.open(packets[1].0, &packets[1].1, &mut out).unwrap();
    }

    #[test]
    fn the_wrong_host_cannot_answer() {
        let (cid, hid, other) = (
            Identity::generate(),
            Identity::generate(),
            Identity::generate(),
        );
        let (init, m1) = Initiator::start(&cid, &hid.public, b"").unwrap();
        // A different host cannot even read message 1 (it was for hid).
        assert!(Responder::read(&other, &m1).is_err());
        // An answer made for another handshake does not finish this one.
        let (_, m1b) = Initiator::start(&cid, &other.public, b"").unwrap();
        let (_, m2b) = Responder::read(&other, &m1b).unwrap().reply(b"").unwrap();
        assert!(init.finish(&m2b).is_err());
    }

    #[test]
    fn a_transport_is_shared_between_threads() {
        let (c, h) = connect();
        let c = std::sync::Arc::new(c);
        let sealed: Vec<(u64, Vec<u8>)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4u8)
                .map(|t| {
                    let c = c.clone();
                    s.spawn(move || {
                        (0..100u8)
                            .map(|i| {
                                let mut buf = [0u8; 64];
                                let (n, len) = c.seal(&[t, i], &mut buf).unwrap();
                                (n, buf[..len].to_vec())
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        // Every counter used once, and every packet opens.
        let mut counters: Vec<u64> = sealed.iter().map(|(n, _)| *n).collect();
        counters.sort_unstable();
        counters.dedup();
        assert_eq!(counters.len(), 400);
        let mut out = [0u8; 64];
        for (n, p) in &sealed {
            h.open(*n, p, &mut out).unwrap();
        }
    }

    #[test]
    fn garbage_handshakes_are_errors() {
        let hid = Identity::generate();
        assert!(Responder::read(&hid, &[]).is_err());
        assert!(Responder::read(&hid, &[0u8; 96]).is_err());
        let (init, _) = Initiator::start(&Identity::generate(), &hid.public, b"").unwrap();
        assert!(init.finish(&[1u8; 48]).is_err());
    }

    #[test]
    fn sessions_use_fresh_keys() {
        // The same two devices, two sessions: a packet from one does not
        // open in the other.
        let (cid, hid) = (Identity::generate(), Identity::generate());
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let (init, m1) = Initiator::start(&cid, &hid.public, b"").unwrap();
            let (h, m2) = Responder::read(&hid, &m1).unwrap().reply(b"").unwrap();
            let (c, _) = init.finish(&m2).unwrap();
            sessions.push((c, h));
        }
        let mut buf = [0u8; 64];
        let mut out = [0u8; 64];
        let (n, len) = sessions[0].0.seal(b"x", &mut buf).unwrap();
        assert!(sessions[1].1.open(n, &buf[..len], &mut out).is_err());
        sessions[0].1.open(n, &buf[..len], &mut out).unwrap();
    }
}
