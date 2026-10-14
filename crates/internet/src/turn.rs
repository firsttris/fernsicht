//! A TURN client (RFC 8656) over UDP, as a state machine: the caller owns
//! the socket and the clock, feeds in what the server sends and sends
//! what is due. Only what Fernsicht needs: one allocation, channels to a
//! few peers (the host's candidates), refreshes, ChannelData.
//!
//! Relayed data goes in ChannelData (4 bytes of header) rather than Send
//! and Data indications (36 bytes and more).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::stun::{
    self, Builder, CHANNEL_MAX, CHANNEL_MIN, Class, Message, TransactionId, attr, method,
};

/// What the client asks for; the server may give less.
const LIFETIME_S: u32 = 600;
/// Channel bindings last 10 minutes, permissions 5 (RFC 8656 sections 12
/// and 9): renewed well before.
const CHANNEL_REFRESH: Duration = Duration::from_secs(4 * 60);
/// A request without an answer goes again this often…
const RETRANSMIT: Duration = Duration::from_millis(250);
/// …this many times before the client gives up (2 s: an allocation has
/// to be quick to be worth it).
const MAX_SENDS: u32 = 8;
/// Most peers a client binds channels to.
pub const MAX_CHANNELS: usize = 8;

/// The long-term credentials a TURN server takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// What a TURN server's URL says (`turn:host:port?transport=udp`). Only
/// UDP over plain `turn:` is used: TCP or TLS would hold every packet
/// behind a lost one.
pub fn parse_url(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("turn:")?;
    let (hostport, query) = rest.split_once('?').unwrap_or((rest, ""));
    let udp = query.is_empty()
        || query
            .split('&')
            .any(|kv| kv.eq_ignore_ascii_case("transport=udp"));
    if !udp {
        return None;
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(':') && p.parse::<u16>().is_ok() => {
            (h.to_owned(), p.parse().ok()?)
        }
        _ => (hostport.to_owned(), 3478),
    };
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .map_or(host.clone(), str::to_owned);
    (!host.is_empty()).then_some((host, port))
}

/// What happened, from the caller's point of view.
#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// Nothing for the caller.
    None,
    /// The allocation is ready: the relayed address to give the peer, and
    /// our own address as the server sees it.
    Allocated {
        relayed: SocketAddr,
        mapped: Option<SocketAddr>,
    },
    /// Data a peer sent us through the relay.
    Data { peer: SocketAddr, data: &'a [u8] },
    /// The allocation failed or was lost.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    Allocate,
    Refresh { lifetime: u32 },
    ChannelBind { channel: u16, peer: SocketAddr },
}

#[derive(Clone, Debug)]
struct Pending {
    tx: TransactionId,
    request: Request,
    sends: u32,
    next: Instant,
}

#[derive(Clone, Debug)]
struct Channel {
    number: u16,
    peer: SocketAddr,
    /// Confirmed by the server: data may go on it.
    bound: bool,
    renew_at: Instant,
}

/// One allocation on one TURN server.
#[derive(Debug)]
pub struct TurnClient {
    server: SocketAddr,
    credentials: Credentials,
    /// Realm and nonce from the server's 401, and the key from them.
    auth: Option<(String, String, [u8; 16])>,
    relayed: Option<SocketAddr>,
    refresh_at: Option<Instant>,
    pending: Vec<Pending>,
    channels: Vec<Channel>,
    outgoing: VecDeque<Vec<u8>>,
    failed: Option<String>,
    tx_seed: [u8; 8],
    tx_count: u32,
}

impl TurnClient {
    /// Starts allocating on `server`; transaction IDs from `seed` (random
    /// in use, fixed in tests).
    pub fn new(server: SocketAddr, credentials: Credentials, seed: [u8; 8], now: Instant) -> Self {
        let mut c = Self {
            server,
            credentials,
            auth: None,
            relayed: None,
            refresh_at: None,
            pending: Vec::new(),
            channels: Vec::new(),
            outgoing: VecDeque::new(),
            failed: None,
            tx_seed: seed,
            tx_count: 0,
        };
        c.request(Request::Allocate, now);
        c
    }

    pub fn server(&self) -> SocketAddr {
        self.server
    }

    /// The relayed address, once allocated.
    pub fn relayed(&self) -> Option<SocketAddr> {
        self.relayed
    }

    /// Why the allocation failed, if it did.
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    fn next_tx(&mut self) -> TransactionId {
        self.tx_count = self.tx_count.wrapping_add(1);
        let mut tx = [0u8; 12];
        tx[..8].copy_from_slice(&self.tx_seed);
        tx[8..].copy_from_slice(&self.tx_count.to_be_bytes());
        tx
    }

    fn encode(&self, tx: TransactionId, request: &Request) -> Vec<u8> {
        let b = match request {
            Request::Allocate => Builder::new(method::ALLOCATE, Class::Request, tx)
                // UDP (17), RFC 8656 section 18.11.
                .attr(attr::REQUESTED_TRANSPORT, &[17, 0, 0, 0])
                .u32_attr(attr::LIFETIME, LIFETIME_S),
            Request::Refresh { lifetime } => Builder::new(method::REFRESH, Class::Request, tx)
                .u32_attr(attr::LIFETIME, *lifetime),
            Request::ChannelBind { channel, peer } => {
                Builder::new(method::CHANNEL_BIND, Class::Request, tx)
                    .attr(
                        attr::CHANNEL_NUMBER,
                        &[(channel >> 8) as u8, *channel as u8, 0, 0],
                    )
                    .xor_address(attr::XOR_PEER_ADDRESS, *peer)
            }
        };
        let b = match &self.auth {
            Some((realm, nonce, key)) => b
                .attr(attr::USERNAME, self.credentials.username.as_bytes())
                .attr(attr::REALM, realm.as_bytes())
                .attr(attr::NONCE, nonce.as_bytes())
                .integrity(key),
            None => b,
        };
        b.fingerprint().finish()
    }

    fn request(&mut self, request: Request, now: Instant) {
        let tx = self.next_tx();
        self.outgoing.push_back(self.encode(tx, &request));
        self.pending.push(Pending {
            tx,
            request,
            sends: 1,
            next: now + RETRANSMIT,
        });
    }

    /// Binds a channel to `peer` (which also lets it send to us); the
    /// channel's number. Data to it may go once [`Self::is_bound`].
    pub fn bind(&mut self, peer: SocketAddr, now: Instant) -> Option<u16> {
        let peer = crate::punch::canonical(peer);
        if let Some(c) = self.channels.iter().find(|c| c.peer == peer) {
            return Some(c.number);
        }
        if self.channels.len() >= MAX_CHANNELS || self.failed.is_some() {
            return None;
        }
        let number = CHANNEL_MIN + self.channels.len() as u16;
        debug_assert!(number <= CHANNEL_MAX);
        self.channels.push(Channel {
            number,
            peer,
            bound: false,
            renew_at: now + CHANNEL_REFRESH,
        });
        if self.relayed.is_some() {
            self.request(
                Request::ChannelBind {
                    channel: number,
                    peer,
                },
                now,
            );
        }
        Some(number)
    }

    /// Whether data to `peer` can go (its channel is bound).
    pub fn is_bound(&self, peer: SocketAddr) -> bool {
        let peer = crate::punch::canonical(peer);
        self.channels.iter().any(|c| c.peer == peer && c.bound)
    }

    /// The channel data to `peer` goes on, once bound.
    pub fn channel_for(&self, peer: SocketAddr) -> Option<u16> {
        let peer = crate::punch::canonical(peer);
        self.channels
            .iter()
            .find(|c| c.peer == peer && c.bound)
            .map(|c| c.number)
    }

    /// The next message to send to the server.
    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.outgoing.pop_front()
    }

    /// Retransmissions and refreshes due at `now`.
    pub fn tick(&mut self, now: Instant) {
        if self.failed.is_some() {
            return;
        }
        let mut gave_up = None;
        let mut resend = Vec::new();
        for p in &mut self.pending {
            if now < p.next {
                continue;
            }
            if p.sends >= MAX_SENDS {
                gave_up = Some(p.request.clone());
                continue;
            }
            p.sends += 1;
            p.next = now + RETRANSMIT;
            resend.push((p.tx, p.request.clone()));
        }
        for (tx, request) in resend {
            let msg = self.encode(tx, &request);
            self.outgoing.push_back(msg);
        }
        if let Some(request) = gave_up {
            self.fail(format!("the TURN server did not answer ({request:?})"));
            return;
        }
        if self.refresh_at.is_some_and(|t| now >= t) {
            self.refresh_at = None;
            self.request(
                Request::Refresh {
                    lifetime: LIFETIME_S,
                },
                now,
            );
        }
        let renew: Vec<(u16, SocketAddr)> = self
            .channels
            .iter_mut()
            .filter(|c| c.bound && now >= c.renew_at)
            .map(|c| {
                c.renew_at = now + CHANNEL_REFRESH;
                (c.number, c.peer)
            })
            .collect();
        for (channel, peer) in renew {
            self.request(Request::ChannelBind { channel, peer }, now);
        }
    }

    /// When [`Self::tick`] next has something to do.
    pub fn next_tick(&self) -> Option<Instant> {
        let pending = self.pending.iter().map(|p| p.next);
        let channels = self.channels.iter().filter(|c| c.bound).map(|c| c.renew_at);
        pending.chain(channels).chain(self.refresh_at).min()
    }

    /// Releases the allocation (a Refresh with lifetime 0); the server
    /// frees it at once instead of after its lifetime.
    pub fn close(&mut self, now: Instant) {
        if self.relayed.is_some() && self.failed.is_none() {
            self.request(Request::Refresh { lifetime: 0 }, now);
        }
    }

    fn fail(&mut self, why: String) {
        self.failed = Some(why);
        self.pending.clear();
        self.relayed = None;
    }

    /// A datagram from the server: a response, or relayed data.
    pub fn handle<'a>(&mut self, datagram: &'a [u8], now: Instant) -> Event<'a> {
        if let Some((number, data)) = stun::parse_channel_data(datagram) {
            return match self.channels.iter().find(|c| c.number == number) {
                Some(c) => Event::Data { peer: c.peer, data },
                None => Event::None,
            };
        }
        let Ok(m) = Message::parse(datagram) else {
            return Event::None;
        };
        if m.class == Class::Indication && m.method == method::DATA {
            // Data from a peer without a channel (before the binding is
            // confirmed).
            return match (m.xor_address(attr::XOR_PEER_ADDRESS), m.attr(attr::DATA)) {
                (Some(peer), Some(data)) => Event::Data {
                    peer: crate::punch::canonical(peer),
                    data,
                },
                _ => Event::None,
            };
        }
        let Some(i) = self.pending.iter().position(|p| p.tx == m.transaction) else {
            return Event::None;
        };
        // An answer to an authenticated request must prove it is from the
        // server that knows the password.
        if let Some((_, _, key)) = &self.auth
            && m.class == Class::Success
            && !m.check_integrity(key)
        {
            return Event::None;
        }
        let pending = self.pending.remove(i);
        match (m.class, pending.request) {
            (Class::Success, Request::Allocate) => self.allocated(&m, now),
            (Class::Success, Request::Refresh { lifetime }) => {
                if lifetime > 0 {
                    self.schedule_refresh(&m, now);
                }
                Event::None
            }
            (Class::Success, Request::ChannelBind { channel, .. }) => {
                if let Some(c) = self.channels.iter_mut().find(|c| c.number == channel) {
                    c.bound = true;
                    c.renew_at = now + CHANNEL_REFRESH;
                }
                Event::None
            }
            (Class::Error, request) => self.error(&m, request, now),
            _ => Event::None,
        }
    }

    fn allocated<'a>(&mut self, m: &Message<'_>, now: Instant) -> Event<'a> {
        let Some(relayed) = m.xor_address(attr::XOR_RELAYED_ADDRESS) else {
            self.fail("the TURN server sent no relayed address".into());
            return Event::Failed(self.failed.clone().unwrap_or_default());
        };
        self.relayed = Some(relayed);
        self.schedule_refresh(m, now);
        // Channels asked for before the allocation was ready.
        let waiting: Vec<(u16, SocketAddr)> =
            self.channels.iter().map(|c| (c.number, c.peer)).collect();
        for (channel, peer) in waiting {
            self.request(Request::ChannelBind { channel, peer }, now);
        }
        Event::Allocated {
            relayed,
            mapped: m.xor_address(attr::XOR_MAPPED_ADDRESS),
        }
    }

    fn schedule_refresh(&mut self, m: &Message<'_>, now: Instant) {
        let lifetime = m.u32_attr(attr::LIFETIME).unwrap_or(LIFETIME_S);
        // A minute before it runs out, or half way for short ones.
        let ahead = u64::from(lifetime)
            .saturating_sub(60)
            .max(u64::from(lifetime) / 2);
        self.refresh_at = Some(now + Duration::from_secs(ahead));
    }

    fn error<'a>(&mut self, m: &Message<'_>, request: Request, now: Instant) -> Event<'a> {
        let (code, reason) = m.error_code().unwrap_or((0, ""));
        let realm = m.text(attr::REALM);
        let nonce = m.text(attr::NONCE);
        match (code, realm, nonce) {
            // The first 401 brings realm and nonce: ask again with them. A
            // 438 (stale nonce) brings a new nonce. A 401 once we had
            // authenticated means the credentials are wrong.
            (401, Some(realm), Some(nonce)) if self.auth.is_none() => {
                let key = stun::long_term_key(
                    &self.credentials.username,
                    realm,
                    &self.credentials.password,
                );
                self.auth = Some((realm.to_owned(), nonce.to_owned(), key));
                self.request(request, now);
                Event::None
            }
            (438, Some(realm), Some(nonce)) => {
                let key = stun::long_term_key(
                    &self.credentials.username,
                    realm,
                    &self.credentials.password,
                );
                self.auth = Some((realm.to_owned(), nonce.to_owned(), key));
                self.request(request, now);
                Event::None
            }
            _ => {
                let why = format!("TURN {request:?} refused: {code} {reason}");
                if matches!(request, Request::Allocate | Request::Refresh { .. }) {
                    self.fail(why.clone());
                    Event::Failed(why)
                } else {
                    log::debug!("{why}");
                    Event::None
                }
            }
        }
    }

    /// A Send indication carrying `data` to `peer`: for a peer without a
    /// bound channel (36 bytes and more of header, instead of 4). A
    /// permission for the peer's address must exist (a channel bound to
    /// any port of it gives one: permissions are per address, RFC 8656
    /// section 9).
    pub fn send_indication(&mut self, peer: SocketAddr, data: &[u8]) -> Vec<u8> {
        let tx = self.next_tx();
        Builder::new(method::SEND, Class::Indication, tx)
            .xor_address(attr::XOR_PEER_ADDRESS, peer)
            .attr(attr::DATA, data)
            .finish()
    }

    /// The ChannelData message carrying `data` to `peer`, into `out`; its
    /// length. `None` while the channel is not bound.
    pub fn wrap(&self, peer: SocketAddr, data: &[u8], out: &mut [u8]) -> Option<usize> {
        let channel = self.channel_for(peer)?;
        let n = stun::CHANNEL_HEADER_LEN + data.len();
        let out = out.get_mut(..n)?;
        out[..stun::CHANNEL_HEADER_LEN].copy_from_slice(&stun::channel_header(channel, data.len()));
        out[stun::CHANNEL_HEADER_LEN..].copy_from_slice(data);
        Some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "198.51.100.1:3478";

    fn creds() -> Credentials {
        Credentials {
            username: "1700000000:fernsicht".into(),
            password: "secret".into(),
        }
    }

    /// The server's side of a request: an answer built for it.
    fn answer(req: &[u8], f: impl FnOnce(Builder) -> Builder, key: Option<&[u8]>) -> Vec<u8> {
        let m = Message::parse(req).unwrap();
        let class = Class::Success;
        let b = f(Builder::new(m.method, class, m.transaction));
        let b = match key {
            Some(k) => b.integrity(k),
            None => b,
        };
        b.fingerprint().finish()
    }

    fn error(req: &[u8], code: u16, nonce: &str) -> Vec<u8> {
        let m = Message::parse(req).unwrap();
        Builder::new(m.method, Class::Error, m.transaction)
            .error(code, "Unauthorized")
            .attr(attr::REALM, b"example.org")
            .attr(attr::NONCE, nonce.as_bytes())
            .finish()
    }

    /// A client through the 401 dance to an allocation.
    fn allocated(now: Instant) -> (TurnClient, [u8; 16]) {
        let mut c = TurnClient::new(SERVER.parse().unwrap(), creds(), [1; 8], now);
        let first = c.poll_transmit().unwrap();
        let m = Message::parse(&first).unwrap();
        assert_eq!((m.method, m.class), (method::ALLOCATE, Class::Request));
        assert!(
            m.attr(attr::MESSAGE_INTEGRITY).is_none(),
            "no credentials yet"
        );
        assert_eq!(c.handle(&error(&first, 401, "n1"), now), Event::None);
        let second = c.poll_transmit().unwrap();
        let key = stun::long_term_key("1700000000:fernsicht", "example.org", "secret");
        let m = Message::parse(&second).unwrap();
        assert!(m.check_integrity(&key) && m.check_fingerprint());
        assert_eq!(m.text(attr::NONCE), Some("n1"));
        let relayed: SocketAddr = "198.51.100.1:50000".parse().unwrap();
        let ok = answer(
            &second,
            |b| {
                b.xor_address(attr::XOR_RELAYED_ADDRESS, relayed)
                    .xor_address(
                        attr::XOR_MAPPED_ADDRESS,
                        "203.0.113.9:40000".parse().unwrap(),
                    )
                    .u32_attr(attr::LIFETIME, 600)
            },
            Some(&key),
        );
        assert_eq!(
            c.handle(&ok, now),
            Event::Allocated {
                relayed,
                mapped: Some("203.0.113.9:40000".parse().unwrap())
            }
        );
        (c, key)
    }

    #[test]
    fn allocates_after_the_challenge_and_binds_channels() {
        let now = Instant::now();
        let (mut c, key) = allocated(now);
        let peer: SocketAddr = "192.0.2.7:47800".parse().unwrap();
        assert_eq!(c.bind(peer, now), Some(CHANNEL_MIN));
        assert_eq!(c.bind(peer, now), Some(CHANNEL_MIN), "once per peer");
        assert!(!c.is_bound(peer));
        let mut out = [0u8; 64];
        assert_eq!(c.wrap(peer, b"hi", &mut out), None, "not bound yet");
        let bind = c.poll_transmit().unwrap();
        let m = Message::parse(&bind).unwrap();
        assert_eq!(m.method, method::CHANNEL_BIND);
        assert_eq!(m.xor_address(attr::XOR_PEER_ADDRESS), Some(peer));
        assert!(m.check_integrity(&key));
        c.handle(&answer(&bind, |b| b, Some(&key)), now);
        assert!(c.is_bound(peer));
        let n = c.wrap(peer, b"hi", &mut out).unwrap();
        assert_eq!(
            stun::parse_channel_data(&out[..n]),
            Some((CHANNEL_MIN, &b"hi"[..]))
        );
        // Relayed data comes back from that peer.
        let mut back = stun::channel_header(CHANNEL_MIN, 3).to_vec();
        back.extend_from_slice(b"yo!");
        assert_eq!(c.handle(&back, now), Event::Data { peer, data: b"yo!" });
        // An answer without the right integrity is ignored.
        let forged = answer(&bind, |b| b, Some(b"wrong"));
        assert_eq!(c.handle(&forged, now), Event::None);
    }

    #[test]
    fn channels_asked_for_early_are_bound_once_allocated() {
        let now = Instant::now();
        let mut c = TurnClient::new(SERVER.parse().unwrap(), creds(), [2; 8], now);
        let peer: SocketAddr = "[2001:db8::1]:47800".parse().unwrap();
        assert_eq!(c.bind(peer, now), Some(CHANNEL_MIN));
        let first = c.poll_transmit().unwrap();
        assert!(c.poll_transmit().is_none(), "no bind before the allocation");
        c.handle(&error(&first, 401, "n"), now);
        let second = c.poll_transmit().unwrap();
        let key = stun::long_term_key("1700000000:fernsicht", "example.org", "secret");
        c.handle(
            &answer(
                &second,
                |b| b.xor_address(attr::XOR_RELAYED_ADDRESS, "198.51.100.1:5".parse().unwrap()),
                Some(&key),
            ),
            now,
        );
        let bind = Message::parse(&c.poll_transmit().unwrap()).unwrap().method;
        assert_eq!(bind, method::CHANNEL_BIND);
    }

    #[test]
    fn retransmits_then_gives_up() {
        let t0 = Instant::now();
        let mut c = TurnClient::new(SERVER.parse().unwrap(), creds(), [3; 8], t0);
        let first = c.poll_transmit().unwrap();
        let mut t = t0;
        for _ in 1..MAX_SENDS {
            t += RETRANSMIT;
            c.tick(t);
            assert_eq!(
                c.poll_transmit().as_deref(),
                Some(&first[..]),
                "the same request again"
            );
        }
        t += RETRANSMIT;
        c.tick(t);
        assert!(c.failure().is_some());
        assert_eq!(c.bind("192.0.2.1:1".parse().unwrap(), t), None);
    }

    #[test]
    fn wrong_credentials_and_stale_nonces() {
        let now = Instant::now();
        let mut c = TurnClient::new(SERVER.parse().unwrap(), creds(), [4; 8], now);
        let first = c.poll_transmit().unwrap();
        c.handle(&error(&first, 401, "n1"), now);
        let second = c.poll_transmit().unwrap();
        // A stale nonce: once more with the new one.
        c.handle(&error(&second, 438, "n2"), now);
        let third = c.poll_transmit().unwrap();
        assert_eq!(
            Message::parse(&third).unwrap().text(attr::NONCE),
            Some("n2")
        );
        // Refused with the credentials: they are wrong.
        assert!(matches!(
            c.handle(&error(&third, 401, "n3"), now),
            Event::Failed(_)
        ));
        assert!(c.failure().is_some());
    }

    #[test]
    fn refreshes_before_the_lifetime_runs_out_and_releases() {
        let now = Instant::now();
        let (mut c, key) = allocated(now);
        assert_eq!(c.next_tick(), Some(now + Duration::from_secs(540)));
        c.tick(now + Duration::from_secs(540));
        let refresh = c.poll_transmit().unwrap();
        let m = Message::parse(&refresh).unwrap();
        assert_eq!(
            (m.method, m.u32_attr(attr::LIFETIME)),
            (method::REFRESH, Some(600))
        );
        c.handle(
            &answer(&refresh, |b| b.u32_attr(attr::LIFETIME, 600), Some(&key)),
            now,
        );
        c.close(now);
        let release = c.poll_transmit().unwrap();
        assert_eq!(
            Message::parse(&release).unwrap().u32_attr(attr::LIFETIME),
            Some(0)
        );
    }

    #[test]
    fn send_indications_carry_data_to_a_peer() {
        let now = Instant::now();
        let (mut c, _) = allocated(now);
        let peer: SocketAddr = "192.0.2.9:61000".parse().unwrap();
        let ind = c.send_indication(peer, b"probe");
        let m = Message::parse(&ind).unwrap();
        assert_eq!((m.method, m.class), (method::SEND, Class::Indication));
        assert_eq!(m.xor_address(attr::XOR_PEER_ADDRESS), Some(peer));
        assert_eq!(m.attr(attr::DATA), Some(&b"probe"[..]));
    }

    #[test]
    fn data_indications_reach_the_caller() {
        let now = Instant::now();
        let (mut c, _) = allocated(now);
        let peer: SocketAddr = "192.0.2.9:47800".parse().unwrap();
        let ind = Builder::new(method::DATA, Class::Indication, [5; 12])
            .xor_address(attr::XOR_PEER_ADDRESS, peer)
            .attr(attr::DATA, b"punch")
            .finish();
        assert_eq!(
            c.handle(&ind, now),
            Event::Data {
                peer,
                data: b"punch"
            }
        );
        assert_eq!(c.handle(b"\x80garbage", now), Event::None);
    }

    #[test]
    fn urls_name_udp_servers_only() {
        assert_eq!(
            parse_url("turn:turn.cloudflare.com:3478?transport=udp"),
            Some(("turn.cloudflare.com".into(), 3478))
        );
        assert_eq!(
            parse_url("turn:203.0.113.1"),
            Some(("203.0.113.1".into(), 3478))
        );
        assert_eq!(
            parse_url("turn:[2001:db8::1]:3479"),
            Some(("2001:db8::1".into(), 3479))
        );
        assert_eq!(parse_url("turn:turn.example:3478?transport=tcp"), None);
        assert_eq!(parse_url("turns:turn.example:5349?transport=tcp"), None);
        assert_eq!(parse_url("stun:stun.cloudflare.com:3478"), None);
        assert_eq!(parse_url("turn:"), None);
    }
}
