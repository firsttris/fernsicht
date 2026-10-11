//! Hosts: addresses, the trusted list, pairing with a PIN and why a host
//! said no.

use std::time::{Duration, Instant};

use anyhow::Context;
use fernsicht_proto::{MAX_DATAGRAM, Packet, Pair as PairMsg, RejectReason};
use fernsicht_secure::pairing::{ClientPairing, PairError};
use fernsicht_secure::{Identity, Peer, PublicKey, Trusted};

use crate::failure::{Failure, FailureCode};
use crate::network::HELLO_INTERVAL;

/// Where this device keeps its key and paired hosts:
/// `$XDG_CONFIG_HOME/fernsicht`, else `~/.config/fernsicht`.
pub fn default_state_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("fernsicht")
}

/// The port hosts listen on unless told otherwise.
pub const DEFAULT_PORT: u16 = 47800;

/// `host` with the default port if it has none (`zentrale.local` →
/// `zentrale.local:47800`; IPv6 needs brackets: `[fe80::1]:47800`).
pub fn with_default_port(host: &str) -> String {
    let has_port = match host.rsplit_once(':') {
        Some((h, p)) => p.parse::<u16>().is_ok() && (!h.contains(':') || h.ends_with(']')),
        None => false,
    };
    if has_port {
        host.to_owned()
    } else {
        format!("{host}:{DEFAULT_PORT}")
    }
}

/// The paired host meant by `what`: its name or its address.
pub fn find_host<'a>(trusted: &'a Trusted, what: &str) -> Option<&'a Peer> {
    // The key first, then the address, the name last and only if it is
    // unique: names are chosen by the other side, and a host named like
    // another one's address must not get its connection.
    let addr = with_default_port(what);
    let by_key = trusted.peers.iter().find(|p| p.key.fingerprint() == what);
    let by_addr = || {
        trusted
            .peers
            .iter()
            .find(|p| p.address.as_deref() == Some(addr.as_str()))
    };
    let by_name = || {
        let mut named = trusted.peers.iter().filter(|p| p.name == what);
        match (named.next(), named.next()) {
            (Some(p), None) => Some(p),
            _ => None,
        }
    };
    by_key.or_else(by_addr).or_else(by_name)
}

/// Whether `what` is an address (an IP, with or without port) rather
/// than a name.
pub fn is_address(what: &str) -> bool {
    with_default_port(what)
        .parse::<std::net::SocketAddr>()
        .is_ok()
}

/// `s` for a terminal: control characters (from names others chose)
/// become `�`.
pub fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// The paired hosts with `key`'s address set to `addr`, after a session
/// with that address went through (the handshake proved it is the host);
/// `None` if nothing changes. Discovery answers are not proof: anyone can
/// answer in a host's name with its (public) key.
pub fn remember_address(trusted: &Trusted, key: &PublicKey, addr: &str) -> Option<Trusted> {
    let peer = trusted.get(key)?;
    if peer.address.as_deref() == Some(addr) {
        return None;
    }
    let mut next = trusted.clone();
    next.peers
        .iter_mut()
        .filter(|p| p.key == *key)
        .for_each(|p| p.address = Some(addr.to_owned()));
    Some(next)
}

/// How long pairing waits for the host.
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(10);

/// Pairs with the host at `host` using the PIN it shows. Returns the host
/// to remember (with its address).
pub fn pair(host: &str, pin: &str, identity: &Identity, name: &str) -> anyhow::Result<Peer> {
    let socket =
        fernsicht_net::socket::connect_udp(host).with_context(|| format!("connect {host}"))?;
    socket.set_read_timeout(Some(Duration::from_millis(50)))?;
    let (mut pairing, msg1) = ClientPairing::start(pin, identity, name);
    let mut out = [0u8; MAX_DATAGRAM];
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + PAIR_TIMEOUT;
    // Step 1 until step 2 comes, then step 3 until step 4: each resent
    // every 250 ms, as UDP may lose them.
    let mut sending: (u8, Vec<u8>) = (1, msg1);
    let mut last_sent: Option<Instant> = None;
    loop {
        if Instant::now() >= deadline {
            return Err(Failure::new(
                FailureCode::NoAnswer,
                format!(
                    "no answer from {host} (is the host running, and pairing open: \"fernsicht-host-agent pair\"?)"
                ),
            )
            .into());
        }
        if last_sent.is_none_or(|t| t.elapsed() >= HELLO_INTERVAL) {
            let n = PairMsg {
                step: sending.0,
                message: &sending.1,
            }
            .encode(&mut out);
            let _ = socket.send(&out[..n]);
            last_sent = Some(Instant::now());
        }
        let Ok(n) = socket.recv(&mut buf) else {
            continue;
        };
        match Packet::decode(&buf[..n]) {
            Ok(Packet::Pair(p)) if p.step == 2 && sending.0 == 1 => {
                let msg3 = pairing.on_reply(p.message).map_err(|e| match e {
                    PairError::WrongPin => Failure::new(FailureCode::WrongPin, "wrong PIN").into(),
                    other => anyhow::anyhow!("pairing failed: {other}"),
                })?;
                sending = (3, msg3);
                last_sent = None;
            }
            Ok(Packet::Pair(p)) if p.step == 4 && sending.0 == 3 => {
                let mut peer = pairing.on_done(p.message)?;
                peer.address = Some(host.to_owned());
                return Ok(peer);
            }
            Ok(Packet::Reject(r)) => return Err(Failure::rejected(r).into()),
            _ => {}
        }
    }
}

/// What a host's rejection means, for people.
pub fn reject_text(r: RejectReason) -> &'static str {
    match r {
        RejectReason::NotPaired => {
            "this device is not paired with the host (on the host: \"fernsicht-host-agent pair\" shows a PIN; here: \"fernsicht-client pair HOST PIN\")"
        }
        RejectReason::PairingClosed => {
            "the host is not in pairing mode (on the host: \"fernsicht-host-agent pair\")"
        }
        RejectReason::TooManyAttempts => "too many wrong PINs: pairing mode closed; open it again",
        RejectReason::Unavailable => {
            "the host could not start a session (no picture or no video encoder; the host agent's log says why)"
        }
    }
}
