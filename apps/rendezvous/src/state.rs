//! What the service keeps: which hosts are registered, which offers wait
//! for an answer, how many offers came lately. Pure; the connection
//! threads share it behind a lock.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use fernsicht_internet::signal::{ErrorCode, IceServer, ServiceToClient};

/// What the service tells a host's thread to do.
#[derive(Debug)]
pub enum ToHost {
    Offer {
        connection: u32,
        offer: String,
        ice_servers: Vec<IceServer>,
    },
    /// Another connection registered under the same ID: this one ends.
    Replaced,
}

struct Host {
    generation: u64,
    tx: Sender<ToHost>,
}

struct Waiting {
    /// The host the offer went to (its ID and registration).
    host: (String, u64),
    tx: Sender<ServiceToClient>,
}

/// How many events a minute each key may have (offers per host, per
/// client address).
#[derive(Default)]
pub struct RateLimit {
    hits: HashMap<String, VecDeque<Instant>>,
}

impl RateLimit {
    /// Counts an event for `key` at `now`; false if it is one too many.
    pub fn take(&mut self, key: &str, per_minute: u32, now: Instant) -> bool {
        let window = Duration::from_secs(60);
        // Forget keys without recent events, so the map stays small.
        self.hits.retain(|_, q| {
            while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
                q.pop_front();
            }
            !q.is_empty()
        });
        let q = self.hits.entry(key.to_owned()).or_default();
        if q.len() >= per_minute as usize {
            return false;
        }
        q.push_back(now);
        true
    }
}

/// Everything the service knows.
#[derive(Default)]
pub struct State {
    hosts: HashMap<String, Host>,
    waiting: HashMap<u32, Waiting>,
    next_connection: u32,
    next_generation: u64,
    pub limits: RateLimit,
}

impl State {
    /// Registers the host `id` (its key proven); returns its registration.
    /// A host already registered under it is told it was replaced.
    pub fn register(&mut self, id: &str, tx: Sender<ToHost>) -> u64 {
        self.next_generation += 1;
        let generation = self.next_generation;
        if let Some(old) = self.hosts.insert(id.to_owned(), Host { generation, tx }) {
            let _ = old.tx.send(ToHost::Replaced);
        }
        generation
    }

    /// Forgets the host `id` if `generation` is still its registration.
    pub fn unregister(&mut self, id: &str, generation: u64) {
        if self
            .hosts
            .get(id)
            .is_some_and(|h| h.generation == generation)
        {
            self.hosts.remove(id);
        }
    }

    pub fn is_registered(&self, id: &str) -> bool {
        self.hosts.contains_key(id)
    }

    /// Passes a client's offer to host `id`; the connection number the
    /// answer will name, or why not.
    pub fn offer(
        &mut self,
        id: &str,
        offer: String,
        ice_servers: Vec<IceServer>,
        client: Sender<ServiceToClient>,
    ) -> Result<u32, ErrorCode> {
        let host = self.hosts.get(id).ok_or(ErrorCode::Offline)?;
        self.next_connection = self.next_connection.wrapping_add(1);
        let connection = self.next_connection;
        host.tx
            .send(ToHost::Offer {
                connection,
                offer,
                ice_servers,
            })
            .map_err(|_| ErrorCode::Offline)?;
        self.waiting.insert(
            connection,
            Waiting {
                host: (id.to_owned(), host.generation),
                tx: client,
            },
        );
        Ok(connection)
    }

    /// The host `(id, generation)` answers `connection`; false if that
    /// offer was not its own (or is no longer waiting).
    pub fn answer(&mut self, host: (&str, u64), connection: u32, msg: ServiceToClient) -> bool {
        let ours = self
            .waiting
            .get(&connection)
            .is_some_and(|w| w.host.0 == host.0 && w.host.1 == host.1);
        if !ours {
            return false;
        }
        let w = self.waiting.remove(&connection).expect("checked above");
        let _ = w.tx.send(msg);
        true
    }

    /// The client of `connection` stopped waiting.
    pub fn give_up(&mut self, connection: u32) {
        self.waiting.remove(&connection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    #[test]
    fn offers_reach_the_registered_host_and_answers_its_client() {
        let mut s = State::default();
        let (host_tx, host_rx) = unbounded();
        let g = s.register("id", host_tx);
        let (client_tx, client_rx) = unbounded();
        let c = s.offer("id", "o".into(), vec![], client_tx).unwrap();
        assert!(
            matches!(host_rx.try_recv(), Ok(ToHost::Offer { connection, .. }) if connection == c)
        );
        // Another host cannot answer it.
        assert!(!s.answer(
            ("other", g),
            c,
            ServiceToClient::Answer { answer: "x".into() }
        ));
        assert!(!s.answer(
            ("id", g + 1),
            c,
            ServiceToClient::Answer { answer: "x".into() }
        ));
        assert!(s.answer(("id", g), c, ServiceToClient::Answer { answer: "a".into() }));
        assert_eq!(
            client_rx.try_recv().unwrap(),
            ServiceToClient::Answer { answer: "a".into() }
        );
        assert!(!s.answer(
            ("id", g),
            c,
            ServiceToClient::Answer {
                answer: "again".into()
            }
        ));
        assert_eq!(
            s.offer("nobody", "o".into(), vec![], unbounded().0),
            Err(ErrorCode::Offline)
        );
    }

    #[test]
    fn a_new_registration_replaces_the_old_one() {
        let mut s = State::default();
        let (old_tx, old_rx) = unbounded();
        let old = s.register("id", old_tx);
        let (new_tx, _new_rx) = unbounded();
        let new = s.register("id", new_tx);
        assert!(matches!(old_rx.try_recv(), Ok(ToHost::Replaced)));
        // The old one leaving does not remove the new one.
        s.unregister("id", old);
        assert!(s.is_registered("id"));
        s.unregister("id", new);
        assert!(!s.is_registered("id"));
    }

    #[test]
    fn rates_are_limited_per_key_and_minute() {
        let mut r = RateLimit::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(r.take("a", 3, t0));
        }
        assert!(!r.take("a", 3, t0));
        assert!(r.take("b", 3, t0), "per key");
        assert!(
            r.take("a", 3, t0 + Duration::from_secs(60)),
            "a minute later"
        );
        assert_eq!(r.hits.len(), 1, "old keys forgotten");
    }
}
