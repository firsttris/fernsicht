//! The rendezvous service's protocol (`docs/internet.md`, “The rendezvous
//! service”): the messages on its WebSockets, the host's key proof, and
//! the sealed offer and answer it passes between a client and its host.
//!
//! The service is written twice (a Rust binary and a Cloudflare Worker);
//! the message types here are the one definition both follow, and the
//! Worker's TypeScript for them is generated from these (ts-rs).

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use fernsicht_secure::exchange;
use fernsicht_secure::{Identity, PublicKey};

use crate::candidates::Candidate;

/// Where a host waits for offers: `/v1/host/<rendezvous ID>`.
pub const HOST_PATH: &str = "/v1/host/";
/// Where a client sends its offer: `/v1/connect/<rendezvous ID>`.
pub const CONNECT_PATH: &str = "/v1/connect/";
/// Largest WebSocket message either side accepts.
pub const MAX_MESSAGE: usize = 16 * 1024;
/// How long the service holds a client's offer waiting for the answer.
pub const ANSWER_TIMEOUT_MS: u64 = 10_000;
/// Most offers a minute the service passes to one host, and from one
/// client address.
pub const OFFERS_PER_MINUTE: u32 = 10;

/// Binds the offer and the answer to this purpose (a Noise prologue).
const EXCHANGE_PROLOGUE: &[u8] = b"fernsicht rendezvous v1";
const ID_CONTEXT: &[u8] = b"fernsicht rendezvous v1";
const PROOF_CONTEXT: &[u8] = b"fernsicht rendezvous proof v1";

/// A host's rendezvous ID: SHA-256 of a context string and its public
/// key, in hex. A client computes it from the key it knows from pairing.
pub fn rendezvous_id(key: &PublicKey) -> String {
    let mut h = Sha256::new();
    h.update(ID_CONTEXT);
    h.update(key.0);
    hex(&h.finalize())
}

/// Whether `s` has the form of a rendezvous ID (64 lower-case hex digits).
pub fn is_rendezvous_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The host's proof that it holds the key it registers with: an HMAC,
/// keyed with the X25519 agreement between its key and the service's
/// ephemeral key, over the service's nonce and the host's key. The
/// service computes the same from its ephemeral secret.
pub fn proof(shared: &[u8; 32], nonce: &[u8], host: &PublicKey) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(shared).expect("HMAC takes any key");
    mac.update(PROOF_CONTEXT);
    mac.update(nonce);
    mac.update(&host.0);
    mac.finalize().into_bytes().into()
}

/// The host's answer to a [`ServiceToHost::Challenge`]; `None` when the
/// service's key is unusable (a low-order point).
pub fn prove(identity: &Identity, ephemeral: &PublicKey, nonce: &[u8]) -> Option<[u8; 32]> {
    let shared = identity.agree(ephemeral)?;
    Some(proof(&shared, nonce, &identity.public))
}

/// A TURN or STUN server, as WebRTC's `RTCIceServer` and Cloudflare's
/// credentials API write it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct IceServer {
    /// `turn:host:port?transport=udp`, `turns:…`, `stun:…`.
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub credential: Option<String>,
}

/// What a host sends the service.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum HostToService {
    /// Registers under the ID of `key` (hex), which the path names.
    Register { key: String },
    /// The key proof (hex), for the challenge.
    Proof { mac: String },
    /// The sealed answer (base64) to offer `connection`.
    Answer { connection: u32, answer: String },
    /// Offer `connection` gets no answer (not from a paired device, or
    /// stale): the client is told at once instead of waiting.
    Refuse { connection: u32 },
}

/// What the service sends a host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ServiceToHost {
    /// Prove the key: the service's ephemeral X25519 key and a nonce (hex).
    Challenge { ephemeral: String, nonce: String },
    /// Registered; `relay`: the service hands out TURN credentials.
    Registered { relay: bool },
    /// A client's sealed offer (base64), and the relay to offer it, if any.
    Offer {
        connection: u32,
        offer: String,
        #[serde(default)]
        ice_servers: Vec<IceServer>,
    },
    /// Something went wrong; the connection closes after it.
    Error { code: ErrorCode, message: String },
}

/// What a client sends the service.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ClientToService {
    /// The sealed offer (base64) for the host the path names.
    Connect { offer: String },
}

/// What the service sends a client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ServiceToClient {
    /// The host's sealed answer (base64).
    Answer {
        answer: String,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

/// Why the service could not pass a message on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ErrorCode {
    /// No host is registered under the ID.
    Offline,
    /// The host did not answer in time.
    Timeout,
    /// The host refused the offer (not paired, or a stale offer).
    Refused,
    /// Too many offers.
    RateLimited,
    /// A malformed message, or one out of turn.
    BadRequest,
    /// The key proof failed.
    Unauthorized,
    /// Another host registered under the same ID (the same key) later.
    Replaced,
}

/// What a client's offer says, sealed for the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferPayload {
    /// The client's wall clock (ms since 1970): a replayed offer is older
    /// than the last one accepted.
    pub time_ms: u64,
    pub candidates: Vec<Candidate>,
}

/// What the host's answer says, sealed for the client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerPayload {
    pub candidates: Vec<Candidate>,
    /// The punch token (hex) both put in their probes.
    pub token: String,
    /// TURN servers with credentials for the client, if the service
    /// hands them out.
    #[serde(default)]
    pub ice_servers: Vec<IceServer>,
}

/// Why an offer or answer could not be used.
#[derive(Debug)]
pub struct SignalError(pub String);

impl std::fmt::Display for SignalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SignalError {}

fn json<T: Serialize>(v: &T) -> Vec<u8> {
    serde_json::to_vec(v).expect("serializable")
}

/// A client's offer to `host`: the state to read the answer with, and the
/// message for the service (base64).
pub fn make_offer(
    identity: &Identity,
    host: &PublicKey,
    payload: &OfferPayload,
) -> Result<(exchange::Offered, String), SignalError> {
    let (offered, msg) = exchange::offer(identity, host, EXCHANGE_PROLOGUE, &json(payload))
        .map_err(|e| SignalError(e.to_string()))?;
    Ok((offered, base64_encode(&msg)))
}

/// The host's side: the opened offer (whose `client` to check), and what
/// it says.
pub fn open_offer(
    identity: &Identity,
    offer: &str,
) -> Result<(exchange::Opened, OfferPayload), SignalError> {
    let msg = base64_decode(offer).ok_or_else(|| SignalError("offer: not base64".into()))?;
    let opened = exchange::open_offer(identity, EXCHANGE_PROLOGUE, &msg)
        .map_err(|e| SignalError(e.to_string()))?;
    let payload =
        serde_json::from_slice(&opened.payload).map_err(|e| SignalError(format!("offer: {e}")))?;
    Ok((opened, payload))
}

/// The host's answer to an opened offer (base64).
pub fn make_answer(
    opened: exchange::Opened,
    payload: &AnswerPayload,
) -> Result<String, SignalError> {
    let msg = opened
        .answer(&json(payload))
        .map_err(|e| SignalError(e.to_string()))?;
    Ok(base64_encode(&msg))
}

/// The client reads the host's answer; fails unless the host wrote it.
pub fn open_answer(offered: exchange::Offered, answer: &str) -> Result<AnswerPayload, SignalError> {
    let msg = base64_decode(answer).ok_or_else(|| SignalError("answer: not base64".into()))?;
    let payload = offered
        .open_answer(&msg)
        .map_err(|e| SignalError(e.to_string()))?;
    serde_json::from_slice(&payload).map_err(|e| SignalError(format!("answer: {e}")))
}

/// Whether an offer from `time_ms` is fresh: newer than the last one
/// accepted from that client (`last_ms`), and not further than 10 minutes
/// from the host's clock either way (clocks differ a little; a recording
/// does not stay fresh).
pub fn offer_is_fresh(time_ms: u64, last_ms: Option<u64>, now_ms: u64) -> bool {
    const WINDOW_MS: u64 = 10 * 60 * 1000;
    last_ms.is_none_or(|last| time_ms > last) && time_ms.abs_diff(now_ms) <= WINDOW_MS
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `s` as exactly 32 bytes of hex.
pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    unhex(s)?.try_into().ok()
}

pub fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Exports the TypeScript types of the messages (for the Worker).
#[cfg(feature = "ts")]
pub fn export_types(cfg: &ts_rs::Config) -> Result<(), ts_rs::ExportError> {
    use ts_rs::TS;
    HostToService::export_all(cfg)?;
    ServiceToHost::export_all(cfg)?;
    ClientToService::export_all(cfg)?;
    ServiceToClient::export_all(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidates::Kind;

    #[test]
    fn ids_come_from_keys() {
        let k = PublicKey([1; 32]);
        let id = rendezvous_id(&k);
        assert!(is_rendezvous_id(&id));
        assert_eq!(id, rendezvous_id(&k));
        assert_ne!(id, rendezvous_id(&PublicKey([2; 32])));
        assert!(!is_rendezvous_id("abc"));
        assert!(!is_rendezvous_id(&id.to_uppercase()));
        // SHA-256 of the context and 32 bytes of 1, as Python's hashlib
        // computes it; the Worker's test checks the same value.
        assert_eq!(
            id,
            "c07924470b2c9f33dd7bd2b8ad14d0cf330f0dab5d29343eb7d8b893474c88a8"
        );
    }

    #[test]
    fn only_the_key_holder_can_prove() {
        let host = Identity::generate();
        let service = Identity::generate();
        let nonce = [9u8; 16];
        let mac = prove(&host, &service.public, &nonce).unwrap();
        // The service computes it from its side of the agreement.
        let shared = service.agree(&host.public).unwrap();
        assert_eq!(mac, proof(&shared, &nonce, &host.public));
        let impostor = Identity::generate();
        let theirs = prove(&impostor, &service.public, &nonce).unwrap();
        assert_ne!(theirs, proof(&shared, &nonce, &host.public));
        assert_ne!(
            mac,
            proof(&shared, &[8u8; 16], &host.public),
            "bound to the nonce"
        );
        assert_eq!(prove(&host, &PublicKey([0; 32]), &nonce), None);
    }

    #[test]
    fn offers_and_answers_go_between_the_paired_devices() {
        let host = Identity::generate();
        let client = Identity::generate();
        let offer = OfferPayload {
            time_ms: 1_700_000_000_000,
            candidates: vec![Candidate {
                kind: Kind::Reflexive,
                addr: "203.0.113.7:40000".parse().unwrap(),
            }],
        };
        let (offered, sealed) = make_offer(&client, &host.public, &offer).unwrap();
        let (opened, got) = open_offer(&host, &sealed).unwrap();
        assert_eq!(opened.client, client.public);
        assert_eq!(got, offer);
        let answer = AnswerPayload {
            candidates: vec![],
            token: hex(&[1; 16]),
            ice_servers: vec![IceServer {
                urls: vec!["turn:turn.example:3478?transport=udp".into()],
                username: Some("u".into()),
                credential: Some("c".into()),
            }],
        };
        let sealed = make_answer(opened, &answer).unwrap();
        assert_eq!(open_answer(offered, &sealed).unwrap(), answer);
        assert!(open_offer(&Identity::generate(), "AAAA").is_err());
        assert!(open_offer(&host, "not base64!").is_err());
    }

    #[test]
    fn offers_must_be_fresh() {
        let now = 1_700_000_000_000;
        assert!(offer_is_fresh(now, None, now));
        assert!(offer_is_fresh(now + 1, Some(now), now));
        assert!(!offer_is_fresh(now, Some(now), now), "replayed");
        assert!(!offer_is_fresh(now - 11 * 60 * 1000, None, now), "too old");
        assert!(
            offer_is_fresh(now + 9 * 60 * 1000, None, now),
            "a clock a little ahead"
        );
    }

    #[test]
    fn messages_look_as_the_worker_expects() {
        let m = ServiceToHost::Offer {
            connection: 3,
            offer: "b64".into(),
            ice_servers: vec![],
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"type":"offer","connection":3,"offer":"b64","iceServers":[]}"#
        );
        let m: HostToService =
            serde_json::from_str(r#"{"type":"answer","connection":3,"answer":"x"}"#).unwrap();
        assert_eq!(
            m,
            HostToService::Answer {
                connection: 3,
                answer: "x".into()
            }
        );
        let e = ServiceToClient::Error {
            code: ErrorCode::RateLimited,
            message: "slow down".into(),
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"type":"error","code":"rateLimited","message":"slow down"}"#
        );
    }

    #[test]
    fn hex_and_base64_roundtrip() {
        assert_eq!(unhex(&hex(&[0, 255, 16])), Some(vec![0, 255, 16]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex32(&hex(&[3; 32])), Some([3; 32]));
        assert_eq!(unhex32(&hex(&[3; 31])), None);
        assert_eq!(base64_decode(&base64_encode(b"xyz")), Some(b"xyz".to_vec()));
    }
}
