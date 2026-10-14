//! Hole punching: when to send probes, and which path to take from the
//! answers (`docs/internet.md`, “Connecting”).
//!
//! Pure: the callers own the socket and the clock and ask what is due.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::candidates::{Candidate, Rung, rung};

/// How long probes are sent before giving up.
pub const PUNCH_FOR: Duration = Duration::from_secs(5);
/// Probes go every this often for the first second…
const FAST: Duration = Duration::from_millis(100);
/// …then this often until [`PUNCH_FOR`].
const SLOW: Duration = Duration::from_millis(500);
/// The most rounds of probes one connection sends: 10 fast and 8 slow.
pub const MAX_ROUNDS: u32 = 18;
/// After the first answer, how long better paths may still answer.
pub const CHOOSE_WITHIN: Duration = Duration::from_millis(150);

/// When probe round `n` (from 0) is due, after the start.
fn round_at(n: u32) -> Duration {
    if n < 10 {
        FAST * n
    } else {
        Duration::from_secs(1) + SLOW * (n - 10)
    }
}

/// Probes towards a set of addresses on a schedule.
#[derive(Clone, Debug)]
pub struct Schedule {
    started: Instant,
    sent: u32,
}

impl Schedule {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            sent: 0,
        }
    }

    /// Whether a round of probes is due at `now`; counts it as sent.
    pub fn due(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.started);
        if self.sent >= MAX_ROUNDS || elapsed >= PUNCH_FOR || elapsed < round_at(self.sent) {
            return false;
        }
        // Rounds missed while the caller was busy are not made up.
        while self.sent < MAX_ROUNDS && round_at(self.sent) <= elapsed {
            self.sent += 1;
        }
        true
    }

    /// The probing is over.
    pub fn finished(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= PUNCH_FOR
    }

    /// How long until the next round is due (for a socket's timeout).
    pub fn next_in(&self, now: Instant) -> Option<Duration> {
        (self.sent < MAX_ROUNDS)
            .then(|| (self.started + round_at(self.sent)).saturating_duration_since(now))
    }
}

/// A path that answered: the other side's candidate, whether it goes
/// through our relay, and when the answer came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answered {
    pub remote: Candidate,
    pub via_relay: bool,
    pub at: Instant,
}

impl Answered {
    pub fn rung(&self) -> Rung {
        rung(&self.remote, self.via_relay)
    }
}

/// The path to take from the answers so far, or `None` to wait: the best
/// one (by the ladder; the earliest among equals) once [`CHOOSE_WITHIN`]
/// passed after the first answer, or at once when it is a LAN path (none
/// can be better).
pub fn choose(answers: &[Answered], now: Instant) -> Option<Answered> {
    let first = answers.iter().map(|a| a.at).min()?;
    let best = answers.iter().min_by_key(|a| (a.rung(), a.at)).copied()?;
    let waited = now.saturating_duration_since(first) >= CHOOSE_WITHIN;
    (waited || best.rung() == Rung::Lan).then_some(best)
}

/// The candidate `from` answered as, among `remotes`: the same address
/// (IPv4 mapped into IPv6 counts as IPv4).
pub fn matching(remotes: &[Candidate], from: SocketAddr) -> Option<Candidate> {
    let from = canonical(from);
    remotes.iter().copied().find(|c| canonical(c.addr) == from)
}

/// `addr` with an IPv4 address mapped into IPv6 written as IPv4.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidates::Kind;

    fn c(kind: Kind, addr: &str) -> Candidate {
        Candidate {
            kind,
            addr: addr.parse().unwrap(),
        }
    }

    #[test]
    fn rounds_come_fast_then_slow_then_stop() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        let mut rounds = Vec::new();
        let mut t = Duration::ZERO;
        while t <= Duration::from_secs(6) {
            if s.due(t0 + t) {
                rounds.push(t.as_millis());
            }
            t += Duration::from_millis(10);
        }
        assert_eq!(rounds.len(), MAX_ROUNDS as usize);
        assert_eq!(&rounds[..3], [0, 100, 200]);
        assert_eq!(&rounds[9..12], [900, 1000, 1500]);
        assert_eq!(*rounds.last().unwrap(), 4500);
        assert!(s.finished(t0 + PUNCH_FOR));
        assert_eq!(s.next_in(t0 + PUNCH_FOR), None);
    }

    #[test]
    fn a_late_caller_does_not_send_a_burst() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        assert!(s.due(t0 + Duration::from_millis(950)));
        assert!(
            !s.due(t0 + Duration::from_millis(960)),
            "missed rounds dropped"
        );
        assert_eq!(
            s.next_in(t0 + Duration::from_millis(960)),
            Some(Duration::from_millis(40))
        );
        assert!(!s.due(t0 + PUNCH_FOR), "over");
    }

    #[test]
    fn the_best_answer_wins_within_the_window() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let punched = Answered {
            remote: c(Kind::Reflexive, "203.0.113.5:61000"),
            via_relay: false,
            at: ms(40),
        };
        let v6 = Answered {
            remote: c(Kind::Host, "[2a02:810::5]:47800"),
            via_relay: false,
            at: ms(120),
        };
        let relayed = Answered {
            remote: c(Kind::Reflexive, "203.0.113.5:61000"),
            via_relay: true,
            at: ms(10),
        };
        assert_eq!(choose(&[], ms(500)), None);
        assert_eq!(choose(&[punched], ms(100)), None, "better ones may come");
        assert_eq!(choose(&[punched, v6], ms(190)), Some(v6));
        assert_eq!(choose(&[relayed, punched], ms(160)), Some(punched));
        assert_eq!(choose(&[relayed], ms(160)), Some(relayed));
        let lan = Answered {
            remote: c(Kind::Host, "192.168.1.2:47800"),
            via_relay: false,
            at: ms(5),
        };
        assert_eq!(choose(&[lan], ms(5)), Some(lan), "nothing beats the LAN");
    }

    #[test]
    fn answers_match_their_candidates() {
        let remotes = [
            c(Kind::Host, "192.168.1.2:47800"),
            c(Kind::Reflexive, "203.0.113.5:61000"),
        ];
        assert_eq!(
            matching(&remotes, "[::ffff:203.0.113.5]:61000".parse().unwrap()),
            Some(remotes[1])
        );
        assert_eq!(
            matching(&remotes, "203.0.113.5:61001".parse().unwrap()),
            None
        );
    }
}
