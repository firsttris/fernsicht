//! Pairing with a 6-digit PIN.
//!
//! The host shows the PIN, the user types it on the client. Both run
//! SPAKE2 with it and get the same key only if the PIN matched; an
//! eavesdropper learns nothing to test PINs against offline, an active
//! attacker gets one guess per attempt. With that key they swap their
//! long-term public keys and names, sealed (ChaCha20-Poly1305) and bound to
//! the SPAKE2 messages.
//!
//! ```text
//! client                               host (pairing mode, PIN shown)
//!   1  spake A, name           ─────►
//!                              ◄─────  2  spake B, seal(host key, name)
//!   3  seal(client key)        ─────►     host stores the client
//!                              ◄─────  4  seal("ok")
//!   client stores the host
//! ```

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};

use crate::{Identity, Peer, PublicKey};

/// Length of a SPAKE2 message (Ed25519 group).
const SPAKE_LEN: usize = 33;
/// Longest device name carried.
pub const MAX_NAME: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    /// The PIN did not match (or someone tampered with the exchange).
    WrongPin,
    /// A message that does not parse.
    Malformed(&'static str),
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairError::WrongPin => f.write_str("wrong PIN"),
            PairError::Malformed(what) => write!(f, "malformed pairing message: {what}"),
        }
    }
}

impl std::error::Error for PairError {}

/// A fresh 6-digit PIN.
pub fn new_pin() -> String {
    let mut b = [0u8; 4];
    crate::random(&mut b);
    // Modulo bias of 2^32 over 10^6 is below 0.03 %; irrelevant here.
    format!("{:06}", u32::from_le_bytes(b) % 1_000_000)
}

fn spake_ids() -> (SpakeId, SpakeId) {
    (
        SpakeId::new(b"fernsicht client"),
        SpakeId::new(b"fernsicht host"),
    )
}

/// Seals `msg` under the SPAKE2 key; `step` makes every nonce unique (each
/// key is used for one pairing only), `aad` binds the SPAKE2 messages.
fn seal(key: &[u8], step: u8, aad: &[u8], msg: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).expect("32-byte key");
    let mut nonce = [0u8; 12];
    nonce[0] = step;
    cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg, aad })
        .expect("sealing cannot fail")
}

fn open(key: &[u8], step: u8, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, PairError> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).expect("32-byte key");
    let mut nonce = [0u8; 12];
    nonce[0] = step;
    cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: sealed, aad })
        .map_err(|_| PairError::WrongPin)
}

fn name_bytes(name: &str) -> Vec<u8> {
    let mut b = name.as_bytes().to_vec();
    // Cut at a character boundary.
    let mut n = b.len().min(MAX_NAME);
    while !name.is_char_boundary(n) {
        n -= 1;
    }
    b.truncate(n);
    b
}

/// Key followed by name: what each side tells the other.
fn key_and_name(key: &PublicKey, name: &str) -> Vec<u8> {
    let mut v = key.0.to_vec();
    v.extend_from_slice(&name_bytes(name));
    v
}

fn parse_key_and_name(b: &[u8]) -> Result<(PublicKey, String), PairError> {
    if b.len() < 32 || b.len() > 32 + MAX_NAME {
        return Err(PairError::Malformed("key and name"));
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&b[..32]);
    let name = String::from_utf8(b[32..].to_vec()).map_err(|_| PairError::Malformed("name"))?;
    Ok((PublicKey(k), name))
}

/// The client's side.
pub struct ClientPairing {
    state: Option<Spake2<Ed25519Group>>,
    key: Option<Vec<u8>>,
    aad: Vec<u8>,
    identity: Identity,
    name: String,
    host: Option<(PublicKey, String)>,
}

impl ClientPairing {
    /// Starts pairing with `pin`; returns message 1.
    pub fn start(pin: &str, identity: &Identity, name: &str) -> (Self, Vec<u8>) {
        let (a, b) = spake_ids();
        let (state, spake) =
            Spake2::<Ed25519Group>::start_a(&Password::new(pin.trim().as_bytes()), &a, &b);
        let mut msg = spake.clone();
        msg.extend_from_slice(&name_bytes(name));
        (
            Self {
                state: Some(state),
                key: None,
                aad: spake,
                identity: identity.clone(),
                name: name.to_owned(),
                host: None,
            },
            msg,
        )
    }

    /// Takes message 2; returns message 3. Fails with
    /// [`PairError::WrongPin`] if the PIN did not match.
    pub fn on_reply(&mut self, msg2: &[u8]) -> Result<Vec<u8>, PairError> {
        if msg2.len() < SPAKE_LEN {
            return Err(PairError::Malformed("message 2"));
        }
        let state = self
            .state
            .take()
            .ok_or(PairError::Malformed("out of order"))?;
        let key = state
            .finish(&msg2[..SPAKE_LEN])
            .map_err(|_| PairError::Malformed("SPAKE2 message"))?;
        self.aad.extend_from_slice(&msg2[..SPAKE_LEN]);
        let host = open(&key, 2, &self.aad, &msg2[SPAKE_LEN..])?;
        self.host = Some(parse_key_and_name(&host)?);
        let reply = seal(
            &key,
            3,
            &self.aad,
            &key_and_name(&self.identity.public, &self.name),
        );
        self.key = Some(key);
        Ok(reply)
    }

    /// Takes message 4: the host stored us. Returns the host to store.
    pub fn on_done(self, msg4: &[u8]) -> Result<Peer, PairError> {
        let key = self.key.ok_or(PairError::Malformed("out of order"))?;
        if open(&key, 4, &self.aad, msg4)? != b"ok" {
            return Err(PairError::Malformed("message 4"));
        }
        let (key, name) = self.host.expect("set with the key");
        Ok(Peer {
            name,
            key,
            address: None,
            paired_at: crate::unix_now(),
        })
    }
}

/// The host's side, for one attempt.
pub struct HostPairing {
    key: Vec<u8>,
    aad: Vec<u8>,
    client_name: String,
}

impl HostPairing {
    /// Takes message 1 (from a client trying `pin`); returns the attempt
    /// and message 2. Whether the PIN matched shows with message 3.
    pub fn respond(
        pin: &str,
        identity: &Identity,
        name: &str,
        msg1: &[u8],
    ) -> Result<(Self, Vec<u8>), PairError> {
        if msg1.len() < SPAKE_LEN || msg1.len() > SPAKE_LEN + MAX_NAME {
            return Err(PairError::Malformed("message 1"));
        }
        let client_name = String::from_utf8(msg1[SPAKE_LEN..].to_vec())
            .map_err(|_| PairError::Malformed("name"))?;
        let (a, b) = spake_ids();
        let (state, spake) =
            Spake2::<Ed25519Group>::start_b(&Password::new(pin.as_bytes()), &a, &b);
        let key = state
            .finish(&msg1[..SPAKE_LEN])
            .map_err(|_| PairError::Malformed("SPAKE2 message"))?;
        let mut aad = msg1[..SPAKE_LEN].to_vec();
        aad.extend_from_slice(&spake);
        let mut msg2 = spake;
        msg2.extend_from_slice(&seal(&key, 2, &aad, &key_and_name(&identity.public, name)));
        Ok((
            Self {
                key,
                aad,
                client_name,
            },
            msg2,
        ))
    }

    /// Takes message 3; returns the client to store and message 4.
    pub fn finish(self, msg3: &[u8]) -> Result<(Peer, Vec<u8>), PairError> {
        let plain = open(&self.key, 3, &self.aad, msg3)?;
        let (key, sent_name) = parse_key_and_name(&plain)?;
        // The name inside the sealed part counts; the clear one was a hint.
        let name = if sent_name.is_empty() {
            self.client_name
        } else {
            sent_name
        };
        Ok((
            Peer {
                name,
                key,
                address: None,
                paired_at: crate::unix_now(),
            },
            seal(&self.key, 4, &self.aad, b"ok"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(
        client_pin: &str,
        host_pin: &str,
    ) -> Result<(Peer, Peer, Identity, Identity), PairError> {
        let (cid, hid) = (Identity::generate(), Identity::generate());
        let (mut c, m1) = ClientPairing::start(client_pin, &cid, "bazzite");
        let (h, m2) = HostPairing::respond(host_pin, &hid, "zentrale", &m1)?;
        let m3 = c.on_reply(&m2)?;
        let (client, m4) = h.finish(&m3)?;
        let host = c.on_done(&m4)?;
        Ok((client, host, cid, hid))
    }

    #[test]
    fn matching_pins_pair_the_devices() {
        let (client, host, cid, hid) = pair("123456", "123456").unwrap();
        assert_eq!((client.key, client.name.as_str()), (cid.public, "bazzite"));
        assert_eq!((host.key, host.name.as_str()), (hid.public, "zentrale"));
        // Spaces around a typed PIN do not matter.
        assert!(pair(" 123456\n", "123456").is_ok());
    }

    #[test]
    fn a_wrong_pin_fails_on_the_client() {
        assert_eq!(pair("123457", "123456").err(), Some(PairError::WrongPin));
    }

    #[test]
    fn tampering_is_detected() {
        let (cid, hid) = (Identity::generate(), Identity::generate());
        let (mut c, m1) = ClientPairing::start("000000", &cid, "c");
        let (h, mut m2) = HostPairing::respond("000000", &hid, "h", &m1).unwrap();
        let last = m2.len() - 1;
        m2[last] ^= 1;
        assert_eq!(c.on_reply(&m2).err(), Some(PairError::WrongPin));

        // A forged message 3 (someone who does not know the key).
        let (h2, _) = HostPairing::respond("000000", &hid, "h", &m1).unwrap();
        assert_eq!(h2.finish(&[0u8; 48]).err(), Some(PairError::WrongPin));
        drop(h);
    }

    #[test]
    fn malformed_messages_are_errors_not_panics() {
        let hid = Identity::generate();
        assert!(HostPairing::respond("1", &hid, "h", &[]).is_err());
        assert!(HostPairing::respond("1", &hid, "h", &[0; 200]).is_err());
        assert!(
            HostPairing::respond("1", &hid, "h", &[0; 33]).is_err(),
            "not a point"
        );
        let (mut c, _) = ClientPairing::start("1", &Identity::generate(), "c");
        assert!(c.on_reply(&[1, 2, 3]).is_err());
        let (c2, _) = ClientPairing::start("1", &Identity::generate(), "c");
        assert!(c2.on_done(b"ok").is_err(), "out of order");
    }

    #[test]
    fn pins_are_six_digits_and_vary() {
        let pins: Vec<String> = (0..20).map(|_| new_pin()).collect();
        assert!(
            pins.iter()
                .all(|p| p.len() == 6 && p.bytes().all(|b| b.is_ascii_digit()))
        );
        assert!(pins.iter().any(|p| p != &pins[0]));
    }

    #[test]
    fn long_names_are_cut_at_a_character() {
        let name = "ä".repeat(40); // 80 bytes
        let b = name_bytes(&name);
        assert_eq!(b.len(), 64);
        assert!(String::from_utf8(b).is_ok());
    }
}
