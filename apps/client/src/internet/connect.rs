//! The client's way to a host over the internet (`docs/internet.md`,
//! “Connecting”): its own candidates (STUN), the sealed offer through the
//! host's rendezvous service, probes to the host's candidates while it
//! probes back, the best path that answers; after 1.5 s without one, the
//! same through a TURN relay.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use fernsicht_internet::candidates::{Candidate, Kind, Rung, host_candidates, offer_list};
use fernsicht_internet::punch::{Answered, Schedule, canonical, choose, matching};
use fernsicht_internet::signal::{self, AnswerPayload, IceServer, OfferPayload};
use fernsicht_internet::stun::{self, Class, Message, TransactionId};
use fernsicht_internet::turn::{self, Credentials, Event, TurnClient};
use fernsicht_internet::{rendezvous, signal::make_offer};
use fernsicht_net::socket::for_socket;
use fernsicht_proto::{PUNCH_TOKEN_LEN, Packet, Punch};
use fernsicht_secure::{Identity, PublicKey};

use super::relay::{self, Relay};
use crate::failure::{Failure, FailureCode};

/// How long the client waits for STUN answers.
const GATHER_FOR: Duration = Duration::from_millis(500);
/// Without an answer by then, the relay is tried too.
const RELAY_AFTER: Duration = Duration::from_millis(1500);
/// The whole attempt, the rendezvous and a relay included.
const GIVE_UP_AFTER: Duration = Duration::from_secs(20);

/// How to connect.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// STUN servers (`host:port`).
    pub stun: Vec<String>,
    /// Through the relay only, at once (to check it).
    pub relay_only: bool,
}

/// How the client reached the host.
pub struct Reached {
    /// The session's socket, connected (to the host, or to the relay's
    /// thread).
    pub socket: UdpSocket,
    /// The host's address as the session reaches it (for the logs).
    pub host: SocketAddr,
    pub path: Rung,
    /// The relay's thread, while the session goes through it.
    pub relay: Option<Relay>,
}

fn unreachable(message: impl Into<String>) -> Failure {
    Failure::new(FailureCode::Unreachable, message)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Asks the STUN servers which public address `socket` has.
fn gather(socket: &UdpSocket, servers: &[SocketAddr]) -> Vec<Candidate> {
    let asked: Vec<(TransactionId, SocketAddr)> = servers
        .iter()
        .map(|&server| {
            let mut tx = [0u8; 12];
            fernsicht_secure::random(&mut tx);
            let _ = socket.send_to(&stun::binding_request(tx), for_socket(socket, server));
            (tx, canonical(server))
        })
        .collect();
    let until = Instant::now() + GATHER_FOR;
    let mut found = Vec::new();
    let mut buf = [0u8; 1500];
    while !asked.is_empty() && found.len() < asked.len() && Instant::now() < until {
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let Ok(m) = Message::parse(&buf[..n]) else {
            continue;
        };
        let ours = asked
            .iter()
            .any(|(tx, s)| *tx == m.transaction && *s == canonical(from));
        if ours
            && m.class == Class::Success
            && let Some(addr) = m.reflexive_address()
        {
            found.push(Candidate {
                kind: Kind::Reflexive,
                addr: canonical(addr),
            });
        }
    }
    found
}

/// One offer and its answer through the service.
fn exchange(
    url: &str,
    host: &PublicKey,
    identity: &Identity,
    candidates: Vec<Candidate>,
) -> Result<AnswerPayload, Failure> {
    let payload = OfferPayload {
        time_ms: now_ms(),
        candidates,
    };
    let (offered, sealed) =
        make_offer(identity, host, &payload).map_err(|e| unreachable(e.to_string()))?;
    let answer = rendezvous::connect(url, host, sealed).map_err(|e| match e {
        rendezvous::Error::Service(signal::ErrorCode::Offline, _) => unreachable(
            "the host is not connected to its rendezvous service (is it running and online?)",
        ),
        rendezvous::Error::Service(signal::ErrorCode::Refused, _) => Failure::new(
            FailureCode::NotPaired,
            "the host refused: this device is not paired with it (any more)",
        ),
        other => unreachable(other.to_string()),
    })?;
    signal::open_answer(offered, &answer)
        .map_err(|e| unreachable(format!("the host's answer: {e}")))
}

/// What the host answered, usable: its candidates (real unicast
/// addresses), the token, the relay.
struct Remote {
    candidates: Vec<Candidate>,
    token: [u8; PUNCH_TOKEN_LEN],
    ice_servers: Vec<IceServer>,
}

fn remote(a: AnswerPayload) -> Result<Remote, Failure> {
    let token = signal::unhex(&a.token)
        .and_then(|t| <[u8; PUNCH_TOKEN_LEN]>::try_from(t).ok())
        .ok_or_else(|| unreachable("the host's answer has no punch token"))?;
    let candidates = a
        .candidates
        .into_iter()
        .filter(|c| {
            c.addr.port() != 0 && !c.addr.ip().is_unspecified() && !c.addr.ip().is_multicast()
        })
        .map(|c| Candidate {
            addr: canonical(c.addr),
            ..c
        })
        .take(fernsicht_internet::candidates::MAX_CANDIDATES)
        .collect();
    Ok(Remote {
        candidates,
        token,
        ice_servers: a.ice_servers,
    })
}

fn probe(token: [u8; PUNCH_TOKEN_LEN], reply: bool) -> [u8; Punch::LEN] {
    let mut out = [0u8; Punch::LEN];
    Punch { token, reply }.encode(&mut out);
    out
}

/// The first TURN server over UDP among `servers`, looked up, and its
/// credentials.
fn turn_server(servers: &[IceServer]) -> Option<(SocketAddr, Credentials)> {
    servers.iter().find_map(|s| {
        let credentials = Credentials {
            username: s.username.clone()?,
            password: s.credential.clone()?,
        };
        let addr = s.urls.iter().find_map(|u| {
            let (host, port) = turn::parse_url(u)?;
            std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port))
                .ok()?
                .next()
        })?;
        Some((addr, credentials))
    })
}

/// The relay's progress during the attempt.
enum RelayState {
    None,
    Allocating(TurnClient),
    /// Allocated; the host was told its address and answered.
    Punching {
        turn: TurnClient,
        remote: Remote,
        schedule: Option<Schedule>,
    },
    Failed,
}

/// A socket for the attempt (IPv4 and IPv6), and the candidates to offer
/// from it: its interfaces' addresses and what the STUN servers saw.
fn prepare(options: &Options) -> Result<(UdpSocket, Vec<Candidate>), Failure> {
    let failed = |e: std::io::Error| unreachable(format!("no socket: {e}"));
    let socket = fernsicht_net::socket::bind_udp_dual_stack().map_err(failed)?;
    socket
        .set_read_timeout(Some(Duration::from_millis(10)))
        .map_err(failed)?;
    let local = socket.local_addr().map_err(failed)?;
    let mut candidates: Vec<Candidate> = host_candidates(local.port())
        .into_iter()
        .filter(|c| local.is_ipv6() || c.addr.is_ipv4())
        .collect();
    candidates.extend(gather(&socket, &stun::resolve(&options.stun)));
    Ok((socket, offer_list(candidates)))
}

/// Whether the attempt is over without a path: the probing ended with no
/// answer, and no relay is left to try.
fn hopeless(schedule: &Schedule, relay: &RelayState, first: &Remote, now: Instant) -> bool {
    let no_relay = match relay {
        RelayState::Failed => true,
        RelayState::None => turn_server(&first.ice_servers).is_none(),
        _ => false,
    };
    schedule.finished(now) && no_relay
}

/// Probes through the relay, when they are due.
fn probe_relayed(relay: &mut RelayState, socket: &UdpSocket, now: Instant) {
    let RelayState::Punching {
        turn,
        remote,
        schedule: Some(s),
    } = relay
    else {
        return;
    };
    if !s.due(now) {
        return;
    }
    let p = probe(remote.token, false);
    let mut out = [0u8; 64];
    for c in &remote.candidates {
        if let Some(n) = turn.wrap(c.addr, &p, &mut out) {
            let _ = socket.send_to(&out[..n], for_socket(socket, turn.server()));
        }
    }
}

/// A probe that came directly: a host's answer is a path, its own probe
/// is answered.
fn on_direct(
    p: Punch,
    from: SocketAddr,
    socket: &UdpSocket,
    remotes: &[Candidate],
) -> Option<Candidate> {
    if p.reply {
        return matching(remotes, from);
    }
    let _ = socket.send_to(&probe(p.token, true), for_socket(socket, from));
    None
}

/// Connects to `host` through its rendezvous service at `url`.
pub fn over_internet(
    url: &str,
    host: &PublicKey,
    identity: &Identity,
    options: &Options,
) -> Result<Reached, Failure> {
    let start = Instant::now();
    let (socket, candidates) = prepare(options)?;
    log::info!(
        "connecting over {url} ({} candidates of ours)",
        candidates.len()
    );
    let first = remote(exchange(url, host, identity, candidates.clone())?)?;
    let relay_at = start
        + if options.relay_only {
            Duration::ZERO
        } else {
            RELAY_AFTER
        };
    let mut tokens = vec![first.token];
    let mut schedule = Schedule::new(Instant::now());
    let mut answers: Vec<Answered> = Vec::new();
    let mut relay = RelayState::None;
    let mut buf = vec![0u8; 2048];
    loop {
        let now = Instant::now();
        if let Some(best) = choose(&answers, now) {
            return finish(socket, best, relay);
        }
        if now.duration_since(start) >= GIVE_UP_AFTER || hopeless(&schedule, &relay, &first, now) {
            return Err(unreachable(
                "no path to the host answered (both behind strict NATs, and no relay)",
            ));
        }
        if schedule.due(now) && !options.relay_only {
            let p = probe(first.token, false);
            for c in &first.candidates {
                let _ = socket.send_to(&p, for_socket(&socket, c.addr));
            }
        }
        let ends = (url, host, identity);
        relay = step_relay(
            relay,
            &socket,
            ends,
            (&candidates, &first),
            (relay_at, now),
            &mut tokens,
        );
        probe_relayed(&mut relay, &socket, now);
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let (from, now) = (canonical(from), Instant::now());
        let via_turn = match &relay {
            RelayState::Allocating(t) | RelayState::Punching { turn: t, .. } => {
                canonical(t.server()) == from
            }
            _ => false,
        };
        if via_turn {
            on_relayed(&mut relay, &buf[..n], &socket, &mut answers, now);
        } else if let Ok(Packet::Punch(p)) = Packet::decode(&buf[..n])
            && tokens.contains(&p.token)
            && let Some(remote) = on_direct(p, from, &socket, &remotes_of(&first, &relay))
            && !options.relay_only
        {
            answers.push(Answered {
                remote,
                via_relay: false,
                at: now,
            });
        }
    }
}

/// The host's candidates from both answers.
fn remotes_of(first: &Remote, relay: &RelayState) -> Vec<Candidate> {
    let mut all = first.candidates.clone();
    if let RelayState::Punching { remote, .. } = relay {
        all.extend(remote.candidates.iter().copied());
    }
    all
}

/// A datagram from the TURN server during the attempt.
fn on_relayed(
    relay: &mut RelayState,
    datagram: &[u8],
    socket: &UdpSocket,
    answers: &mut Vec<Answered>,
    now: Instant,
) {
    let (turn, remote) = match relay {
        RelayState::Allocating(t) => (t, None),
        RelayState::Punching { turn, remote, .. } => (turn, Some(&*remote)),
        _ => return,
    };
    let Event::Data { peer, data } = turn.handle(datagram, now) else {
        return;
    };
    let (Some(remote), Ok(Packet::Punch(p))) = (remote, Packet::decode(data)) else {
        return;
    };
    if p.token != remote.token {
        return;
    }
    if p.reply {
        if let Some(c) = matching(&remote.candidates, peer) {
            answers.push(Answered {
                remote: c,
                via_relay: true,
                at: now,
            });
        }
    } else {
        let mut out = [0u8; 64];
        if let Some(n) = turn.wrap(peer, &probe(p.token, true), &mut out) {
            let _ = socket.send_to(&out[..n], for_socket(socket, turn.server()));
        }
    }
}

/// The relay's next step: allocate from `relay_at` on (no answer by
/// then), tell the host its address once allocated, keep it fresh.
fn step_relay(
    relay: RelayState,
    socket: &UdpSocket,
    (url, host, identity): (&str, &PublicKey, &Identity),
    (candidates, first): (&[Candidate], &Remote),
    (relay_at, now): (Instant, Instant),
    tokens: &mut Vec<[u8; PUNCH_TOKEN_LEN]>,
) -> RelayState {
    let send_all = |turn: &mut TurnClient| {
        while let Some(m) = turn.poll_transmit() {
            let _ = socket.send_to(&m, for_socket(socket, turn.server()));
        }
    };
    match relay {
        RelayState::None if now >= relay_at => {
            let Some((server, credentials)) = turn_server(&first.ice_servers) else {
                return RelayState::Failed;
            };
            log::info!("no direct path yet: trying the relay {server}");
            let mut seed = [0u8; 8];
            fernsicht_secure::random(&mut seed);
            let mut turn = TurnClient::new(server, credentials, seed, now);
            send_all(&mut turn);
            RelayState::Allocating(turn)
        }
        RelayState::Allocating(mut turn) => {
            turn.tick(now);
            send_all(&mut turn);
            if let Some(why) = turn.failure() {
                log::warn!("relay: {why}");
                return RelayState::Failed;
            }
            let Some(relayed) = turn.relayed() else {
                return RelayState::Allocating(turn);
            };
            // The host learns the relay's address with a second offer,
            // and limits the session to it.
            let mut with_relay = candidates.to_vec();
            with_relay.push(Candidate {
                kind: Kind::Relayed,
                addr: relayed,
            });
            let second = exchange(url, host, identity, offer_list(with_relay)).and_then(remote);
            let remote = match second {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("relay: {e}");
                    return RelayState::Failed;
                }
            };
            tokens.push(remote.token);
            for c in &remote.candidates {
                turn.bind(c.addr, now);
            }
            send_all(&mut turn);
            RelayState::Punching {
                turn,
                remote,
                schedule: None,
            }
        }
        RelayState::Punching {
            mut turn,
            remote,
            mut schedule,
        } => {
            turn.tick(now);
            send_all(&mut turn);
            // Probes once the channels are bound.
            if schedule.is_none() && remote.candidates.iter().any(|c| turn.is_bound(c.addr)) {
                schedule = Some(Schedule::new(now));
            }
            if turn.failure().is_some() {
                return RelayState::Failed;
            }
            RelayState::Punching {
                turn,
                remote,
                schedule,
            }
        }
        other => other,
    }
}

/// The session's socket on the chosen path.
fn finish(socket: UdpSocket, best: Answered, relay: RelayState) -> Result<Reached, Failure> {
    let path = best.rung();
    let host = best.remote.addr;
    log::info!("reached the host at {host} ({path:?})");
    if best.via_relay {
        let RelayState::Punching { turn, .. } = relay else {
            return Err(unreachable("the relay is gone"));
        };
        let (session, relay) =
            relay::spawn(socket, turn, host).map_err(|e| unreachable(e.to_string()))?;
        return Ok(Reached {
            socket: session,
            host,
            path,
            relay: Some(relay),
        });
    }
    // A relay allocated meanwhile is released.
    if let RelayState::Allocating(mut turn) | RelayState::Punching { mut turn, .. } = relay {
        turn.close(Instant::now());
        while let Some(m) = turn.poll_transmit() {
            let _ = socket.send_to(&m, for_socket(&socket, turn.server()));
        }
    }
    socket
        .connect(for_socket(&socket, host))
        .map_err(|e| unreachable(e.to_string()))?;
    Ok(Reached {
        socket,
        host,
        path,
        relay: None,
    })
}

/// Whether the host answers at `target` in the LAN (or a VPN) right now:
/// a discovery question to that address, its key in the answer. Returns
/// at the first right answer, else after `wait`.
pub fn answers_directly(target: &str, key: &PublicKey, wait: Duration) -> bool {
    use fernsicht_proto::Discover;
    let Some(addr) = std::net::ToSocketAddrs::to_socket_addrs(target)
        .ok()
        .and_then(|mut a| a.next())
    else {
        return false;
    };
    let Ok(socket) = UdpSocket::bind(fernsicht_net::socket::local_for(&addr)) else {
        return false;
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(20)));
    let mut nonce = [0u8; 8];
    fernsicht_secure::random(&mut nonce);
    let nonce = u64::from_le_bytes(nonce);
    let mut question = [0u8; Discover::LEN];
    Discover { nonce }.encode(&mut question);
    let until = Instant::now() + wait;
    let mut asked = Instant::now() - Duration::from_secs(1);
    let mut buf = [0u8; 1500];
    while Instant::now() < until {
        // Again now and then: one question may get lost on Wi-Fi.
        if asked.elapsed() >= wait / 3 {
            let _ = socket.send_to(&question, addr);
            asked = Instant::now();
        }
        let Ok((n, _)) = socket.recv_from(&mut buf) else {
            continue;
        };
        if let Ok(Packet::Announce(a)) = Packet::decode(&buf[..n])
            && a.nonce == nonce
            && a.key == key.0
        {
            return true;
        }
    }
    false
}
