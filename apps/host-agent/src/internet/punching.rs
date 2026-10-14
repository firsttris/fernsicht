//! The host's part of connecting over the internet, in the control loop
//! (it owns the socket): an offer is checked, the host's public address
//! asked of STUN servers, the sealed answer written, and probes sent to
//! the client's candidates while it punches back (`docs/internet.md`,
//! “Connecting”).
//!
//! Pure: what to send comes back as [`Action`]s, which the control loop
//! carries out.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use fernsicht_internet::candidates::{Candidate, Kind, MAX_CANDIDATES, offer_list};
use fernsicht_internet::punch::{PUNCH_FOR, Schedule, canonical};
use fernsicht_internet::signal::{
    self, AnswerPayload, IceServer, make_answer, offer_is_fresh, open_offer,
};
use fernsicht_internet::stun::{self, Class, Message, TransactionId};
use fernsicht_proto::{PUNCH_TOKEN_LEN, Punch};
use fernsicht_secure::exchange::Opened;
use fernsicht_secure::{Identity, PublicKey};

use super::signaling::{Offer, Reply};

/// How long the host waits for STUN answers before answering without.
pub(crate) const GATHER_FOR: Duration = Duration::from_millis(500);
/// Most connections being set up at once.
const MAX_JOBS: usize = 4;
/// How long after the punching a relayed address is remembered (its
/// handshake comes once a path is chosen).
const RELAYED_FOR: Duration = Duration::from_secs(30);

/// What the control loop does for the punching.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Send { to: SocketAddr, data: Vec<u8> },
    Reply(Reply),
}

struct Job {
    connection: u32,
    /// The opened offer, until the answer is written.
    opened: Option<Opened>,
    remotes: Vec<Candidate>,
    token: [u8; PUNCH_TOKEN_LEN],
    ice_servers: Vec<IceServer>,
    local: Vec<Candidate>,
    /// STUN requests without an answer yet: transaction and server.
    asking: Vec<(TransactionId, SocketAddr)>,
    gather_until: Instant,
    schedule: Option<Schedule>,
}

/// The connections being set up.
#[derive(Default)]
pub(crate) struct Punching {
    jobs: Vec<Job>,
    /// The newest offer time accepted per client (its clock, ms).
    offer_times: HashMap<PublicKey, u64>,
    /// Relayed addresses clients offered, until when.
    relayed: Vec<(SocketAddr, Instant)>,
}

/// Whether a client's candidate is one to send to: a real unicast address
/// with a port.
fn sendable(c: &Candidate) -> bool {
    let ip = c.addr.ip().to_canonical();
    c.addr.port() != 0
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && ip != std::net::Ipv4Addr::BROADCAST
}

impl Punching {
    /// An offer from the service: checked, then STUN is asked. `local`:
    /// the host's own candidates; `now_ms`: the wall clock.
    pub(crate) fn offer(
        &mut self,
        offer: Offer,
        identity: &Identity,
        is_paired: impl Fn(&PublicKey) -> bool,
        local: Vec<Candidate>,
        (now, now_ms): (Instant, u64),
    ) -> Vec<Action> {
        let connection = offer.incoming.connection;
        let refuse = vec![Action::Reply(Reply::Refuse { connection })];
        let Ok((opened, payload)) = open_offer(identity, &offer.incoming.offer) else {
            log::debug!("rendezvous: an offer that is not for this host");
            return refuse;
        };
        if !is_paired(&opened.client) {
            log::warn!(
                "rendezvous: an offer from an unpaired device ({})",
                opened.client.fingerprint()
            );
            return refuse;
        }
        let last = self.offer_times.get(&opened.client).copied();
        if !offer_is_fresh(payload.time_ms, last, now_ms) {
            log::warn!("rendezvous: a stale or replayed offer dropped");
            return refuse;
        }
        self.jobs
            .retain(|j| j.schedule.as_ref().is_none_or(|s| !s.finished(now)));
        if self.jobs.len() >= MAX_JOBS {
            log::warn!("rendezvous: too many connections being set up");
            return refuse;
        }
        self.offer_times.insert(opened.client, payload.time_ms);
        let remotes: Vec<Candidate> = payload
            .candidates
            .into_iter()
            .filter(sendable)
            .take(MAX_CANDIDATES)
            .map(|c| Candidate {
                addr: canonical(c.addr),
                ..c
            })
            .collect();
        let until = now + PUNCH_FOR + RELAYED_FOR;
        self.relayed.retain(|(_, t)| *t > now);
        self.relayed.extend(
            remotes
                .iter()
                .filter(|c| c.kind == Kind::Relayed)
                .map(|c| (c.addr, until)),
        );
        let mut token = [0u8; PUNCH_TOKEN_LEN];
        fernsicht_secure::random(&mut token);
        let mut actions = Vec::new();
        let asking = offer
            .stun
            .iter()
            .map(|&server| {
                let mut tx = [0u8; 12];
                fernsicht_secure::random(&mut tx);
                actions.push(Action::Send {
                    to: server,
                    data: stun::binding_request(tx),
                });
                (tx, server)
            })
            .collect();
        log::info!(
            "rendezvous: {} asks to connect ({} candidates)",
            opened.client.fingerprint(),
            remotes.len()
        );
        self.jobs.push(Job {
            connection,
            opened: Some(opened),
            remotes,
            token,
            ice_servers: offer.incoming.ice_servers,
            local,
            asking,
            gather_until: now + GATHER_FOR,
            schedule: None,
        });
        // Without STUN servers the answer goes at once.
        actions.extend(self.tick(now));
        actions
    }

    /// A STUN answer on the host's socket.
    pub(crate) fn stun_answer(
        &mut self,
        datagram: &[u8],
        from: SocketAddr,
        now: Instant,
    ) -> Vec<Action> {
        let Ok(m) = Message::parse(datagram) else {
            return Vec::new();
        };
        if m.class != Class::Success || m.method != stun::method::BINDING {
            return Vec::new();
        }
        let from = canonical(from);
        for job in &mut self.jobs {
            let Some(i) = job
                .asking
                .iter()
                .position(|(tx, server)| *tx == m.transaction && canonical(*server) == from)
            else {
                continue;
            };
            job.asking.remove(i);
            if let Some(addr) = m.reflexive_address() {
                job.local.push(Candidate {
                    kind: Kind::Reflexive,
                    addr: canonical(addr),
                });
            }
        }
        self.tick(now)
    }

    /// What is due: answers once STUN answered (or its time is up), probes
    /// while punching.
    pub(crate) fn tick(&mut self, now: Instant) -> Vec<Action> {
        let mut actions = Vec::new();
        for job in &mut self.jobs {
            if let Some(opened) = job
                .opened
                .take_if(|_| job.asking.is_empty() || now >= job.gather_until)
            {
                let payload = AnswerPayload {
                    candidates: offer_list(job.local.iter().copied()),
                    token: signal::hex(&job.token),
                    ice_servers: std::mem::take(&mut job.ice_servers),
                };
                match make_answer(opened, &payload) {
                    Ok(answer) => {
                        actions.push(Action::Reply(Reply::Answer {
                            connection: job.connection,
                            answer,
                        }));
                        job.schedule = Some(Schedule::new(now));
                    }
                    Err(e) => {
                        log::warn!("rendezvous: answer: {e}");
                        actions.push(Action::Reply(Reply::Refuse {
                            connection: job.connection,
                        }));
                    }
                }
            }
            if let Some(s) = job.schedule.as_mut()
                && s.due(now)
            {
                let mut data = vec![0u8; Punch::LEN];
                Punch {
                    token: job.token,
                    reply: false,
                }
                .encode(&mut data);
                actions.extend(job.remotes.iter().map(|c| Action::Send {
                    to: c.addr,
                    data: data.clone(),
                }));
            }
        }
        // Answered or failed jobs whose probing is over go.
        self.jobs.retain(|j| {
            j.opened.is_some() || j.schedule.as_ref().is_some_and(|s| !s.finished(now))
        });
        actions
    }

    /// A probe from a client: answered over the same path if its token is
    /// one of ours.
    pub(crate) fn punch(&self, p: Punch, from: SocketAddr) -> Vec<Action> {
        let ours = self
            .jobs
            .iter()
            .any(|j| j.schedule.is_some() && j.token == p.token);
        if p.reply || !ours {
            return Vec::new();
        }
        let mut data = vec![0u8; Punch::LEN];
        Punch {
            token: p.token,
            reply: true,
        }
        .encode(&mut data);
        vec![Action::Send { to: from, data }]
    }

    /// Whether `addr` is a relay a client offered lately: its session gets
    /// the relay's limits.
    pub(crate) fn is_relayed(&self, addr: SocketAddr, now: Instant) -> bool {
        let addr = canonical(addr);
        self.relayed
            .iter()
            .any(|(a, until)| *a == addr && *until > now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fernsicht_internet::rendezvous::IncomingOffer;
    use fernsicht_internet::signal::{OfferPayload, make_offer, open_answer};

    fn c(kind: Kind, addr: &str) -> Candidate {
        Candidate {
            kind,
            addr: addr.parse().unwrap(),
        }
    }

    const NOW_MS: u64 = 1_700_000_000_000;

    struct Setup {
        host: Identity,
        client: Identity,
        punching: Punching,
    }

    fn setup() -> Setup {
        Setup {
            host: Identity::generate(),
            client: Identity::generate(),
            punching: Punching::default(),
        }
    }

    fn offer(
        s: &Setup,
        time_ms: u64,
        candidates: Vec<Candidate>,
        stun: Vec<SocketAddr>,
    ) -> (fernsicht_secure::exchange::Offered, Offer) {
        let (offered, sealed) = make_offer(
            &s.client,
            &s.host.public,
            &OfferPayload {
                time_ms,
                candidates,
            },
        )
        .unwrap();
        let incoming = IncomingOffer {
            connection: 7,
            offer: sealed,
            ice_servers: vec![IceServer {
                urls: vec!["turn:t:3478".into()],
                username: Some("u".into()),
                credential: Some("c".into()),
            }],
        };
        (offered, Offer { incoming, stun })
    }

    fn sends(actions: &[Action]) -> Vec<(SocketAddr, &[u8])> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send { to, data } => Some((*to, data.as_slice())),
                Action::Reply(_) => None,
            })
            .collect()
    }

    fn answer(actions: &[Action]) -> Option<&str> {
        actions.iter().find_map(|a| match a {
            Action::Reply(Reply::Answer { answer, .. }) => Some(answer.as_str()),
            _ => None,
        })
    }

    #[test]
    fn an_offer_is_answered_with_the_stun_address_and_punched() {
        let mut s = setup();
        let t0 = Instant::now();
        let stun_server: SocketAddr = "198.51.100.1:3478".parse().unwrap();
        let client_public = c(Kind::Reflexive, "203.0.113.9:40000");
        let (offered, o) = offer(&s, NOW_MS, vec![client_public], vec![stun_server]);
        let local = vec![c(Kind::Host, "192.168.1.20:47800")];
        let host = s.host.clone();
        let actions = s.punching.offer(o, &host, |_| true, local, (t0, NOW_MS));
        // A STUN request first, no answer yet.
        let [(to, request)] = sends(&actions)[..] else {
            panic!("{actions:?}")
        };
        assert_eq!(to, stun_server);
        assert!(answer(&actions).is_none());
        // The server's answer: the host's public address.
        let m = Message::parse(request).unwrap();
        let reply = stun::Builder::new(stun::method::BINDING, Class::Success, m.transaction)
            .xor_address(
                stun::attr::XOR_MAPPED_ADDRESS,
                "198.51.100.77:47800".parse().unwrap(),
            )
            .finish();
        let t1 = t0 + Duration::from_millis(30);
        let actions = s.punching.stun_answer(&reply, stun_server, t1);
        let payload = open_answer(offered, answer(&actions).unwrap()).unwrap();
        assert_eq!(
            payload.candidates,
            [
                c(Kind::Host, "192.168.1.20:47800"),
                c(Kind::Reflexive, "198.51.100.77:47800")
            ]
        );
        assert_eq!(payload.ice_servers.len(), 1, "the relay goes to the client");
        // The first probes go out with the answer.
        let probes = sends(&actions);
        assert_eq!(probes.len(), 1);
        assert_eq!(probes[0].0, client_public.addr);
        let Ok(fernsicht_proto::Packet::Punch(p)) = fernsicht_proto::Packet::decode(probes[0].1)
        else {
            panic!()
        };
        assert_eq!(signal::hex(&p.token), payload.token);
        // The client's probe with that token is answered; others are not.
        let from: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        assert_eq!(sends(&s.punching.punch(p, from)).len(), 1);
        let reply = Punch { reply: true, ..p };
        assert!(
            s.punching.punch(reply, from).is_empty(),
            "replies are not answered"
        );
        let other = Punch {
            token: [0; 16],
            reply: false,
        };
        assert!(s.punching.punch(other, from).is_empty());
        // Probes keep coming, then stop.
        assert_eq!(
            sends(&s.punching.tick(t1 + Duration::from_millis(100))).len(),
            1
        );
        assert!(s.punching.tick(t1 + PUNCH_FOR).is_empty());
        assert!(s.punching.jobs.is_empty());
    }

    #[test]
    fn without_stun_the_answer_waits_no_longer_than_the_gathering() {
        let mut s = setup();
        let t0 = Instant::now();
        let (_, o) = offer(
            &s,
            NOW_MS,
            vec![],
            vec!["198.51.100.1:3478".parse().unwrap()],
        );
        let host = s.host.clone();
        let actions = s.punching.offer(o, &host, |_| true, vec![], (t0, NOW_MS));
        assert!(answer(&actions).is_none());
        assert!(answer(&s.punching.tick(t0 + GATHER_FOR)).is_some());
        let (_, o) = offer(&s, NOW_MS + 1, vec![], vec![]);
        let actions = s.punching.offer(o, &host, |_| true, vec![], (t0, NOW_MS));
        assert!(answer(&actions).is_some(), "no servers: at once");
    }

    #[test]
    fn strangers_replays_and_bad_candidates_are_refused() {
        let mut s = setup();
        let t0 = Instant::now();
        let host = s.host.clone();
        let refused = |a: &[Action]| a == [Action::Reply(Reply::Refuse { connection: 7 })];
        let (_, o) = offer(&s, NOW_MS, vec![], vec![]);
        assert!(refused(&s.punching.offer(
            o,
            &host,
            |_| false,
            vec![],
            (t0, NOW_MS)
        )));
        let (_, o) = offer(&s, NOW_MS, vec![], vec![]);
        assert!(!refused(&s.punching.offer(
            o,
            &host,
            |_| true,
            vec![],
            (t0, NOW_MS)
        )));
        let (_, o) = offer(&s, NOW_MS, vec![], vec![]);
        assert!(
            refused(&s.punching.offer(o, &host, |_| true, vec![], (t0, NOW_MS))),
            "replayed"
        );
        let (_, o) = offer(&s, NOW_MS - 3_600_000, vec![], vec![]);
        assert!(
            refused(&s.punching.offer(o, &host, |_| true, vec![], (t0, NOW_MS))),
            "old"
        );
        // Not for this host.
        let other = Identity::generate();
        let (_, o) = offer(&s, NOW_MS + 5, vec![], vec![]);
        assert!(refused(&s.punching.offer(
            o,
            &other,
            |_| true,
            vec![],
            (t0, NOW_MS)
        )));
        // Only real unicast addresses with a port are punched.
        let bad = vec![
            c(Kind::Host, "0.0.0.0:47800"),
            c(Kind::Host, "224.0.0.1:47800"),
            c(Kind::Host, "255.255.255.255:47800"),
            c(Kind::Host, "192.168.1.5:0"),
            c(Kind::Relayed, "198.51.100.1:50000"),
        ];
        let (_, o) = offer(&s, NOW_MS + 6, bad, vec![]);
        let actions = s.punching.offer(o, &host, |_| true, vec![], (t0, NOW_MS));
        let targets: Vec<SocketAddr> = sends(&actions).iter().map(|(t, _)| *t).collect();
        assert_eq!(
            targets,
            ["198.51.100.1:50000".parse::<SocketAddr>().unwrap()]
        );
        assert!(
            s.punching
                .is_relayed("198.51.100.1:50000".parse().unwrap(), t0)
        );
        assert!(
            s.punching
                .is_relayed("[::ffff:198.51.100.1]:50000".parse().unwrap(), t0)
        );
        assert!(!s.punching.is_relayed(
            "198.51.100.1:50000".parse().unwrap(),
            t0 + PUNCH_FOR + RELAYED_FOR
        ));
    }

    #[test]
    fn only_a_few_connections_are_set_up_at_once() {
        let mut s = setup();
        let t0 = Instant::now();
        let host = s.host.clone();
        let results: Vec<bool> = (0..=MAX_JOBS as u64)
            .map(|i| {
                let (_, o) = offer(&s, NOW_MS + i, vec![], vec![]);
                let a = s.punching.offer(o, &host, |_| true, vec![], (t0, NOW_MS));
                answer(&a).is_some()
            })
            .collect();
        assert_eq!(results.iter().filter(|ok| **ok).count(), MAX_JOBS);
        // Once their probing is over, there is room again.
        let later = t0 + PUNCH_FOR;
        let (_, o) = offer(&s, NOW_MS + 100, vec![], vec![]);
        assert!(
            answer(
                &s.punching
                    .offer(o, &host, |_| true, vec![], (later, NOW_MS))
            )
            .is_some()
        );
    }
}
