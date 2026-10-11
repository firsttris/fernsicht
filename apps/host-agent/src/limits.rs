//! Rate limits for what anyone on the network can ask the host for
//! (discovery answers, handshakes): per source address, so one sender
//! cannot use up what the others get, and in total, so all together stay
//! bounded.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// A token bucket: `rate` tokens a second, at most `rate` at once.
#[derive(Clone, Copy, Debug)]
pub struct Bucket {
    rate: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    pub fn new(rate: f64, now: Instant) -> Self {
        Self {
            rate,
            tokens: rate,
            at: now,
        }
    }

    /// Takes a token if there is one.
    pub fn take(&mut self, now: Instant) -> bool {
        let refill = now.saturating_duration_since(self.at).as_secs_f64() * self.rate;
        self.tokens = (self.tokens + refill).min(self.rate);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Full again (nothing taken for a second): forgettable.
    fn idle(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) >= Duration::from_secs(1)
    }
}

/// Sources tracked at most; past it, idle ones are forgotten, and new ones
/// wait (the total still bounds them).
const MAX_SOURCES: usize = 1024;

/// A bucket per source address and one for all.
#[derive(Debug)]
pub struct PerSource {
    per_source: f64,
    total: Bucket,
    sources: HashMap<IpAddr, Bucket>,
}

impl PerSource {
    pub fn new(per_source: f64, total: f64, now: Instant) -> Self {
        Self {
            per_source,
            total: Bucket::new(total, now),
            sources: HashMap::new(),
        }
    }

    /// Whether `from` may be answered now.
    pub fn take(&mut self, from: IpAddr, now: Instant) -> bool {
        if !self.sources.contains_key(&from) && self.sources.len() >= MAX_SOURCES {
            self.sources.retain(|_, b| !b.idle(now));
            if self.sources.len() >= MAX_SOURCES {
                return false;
            }
        }
        let rate = self.per_source;
        let source = self
            .sources
            .entry(from)
            .or_insert_with(|| Bucket::new(rate, now));
        // The source's token first: a flooding source does not drain the
        // total.
        source.take(now) && self.total.take(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([192, 168, 1, n])
    }

    #[test]
    fn a_bucket_refills_at_its_rate() {
        let t = Instant::now();
        let mut b = Bucket::new(20.0, t);
        assert_eq!((0..25).filter(|_| b.take(t)).count(), 20);
        assert!(!b.take(t + Duration::from_millis(10)));
        assert!(b.take(t + Duration::from_millis(60)));
    }

    #[test]
    fn one_flooding_source_does_not_silence_the_others() {
        let t = Instant::now();
        let mut l = PerSource::new(20.0, 200.0, t);
        let flood = (0..1000).filter(|_| l.take(ip(66), t)).count();
        assert_eq!(flood, 20);
        assert!(l.take(ip(87), t), "another client is still answered");
    }

    #[test]
    fn all_sources_together_stay_bounded() {
        let t = Instant::now();
        let mut l = PerSource::new(20.0, 200.0, t);
        let answered = (0..250u16)
            .filter(|n| l.take(IpAddr::from([10, 0, (n / 256) as u8, (n % 256) as u8]), t))
            .count();
        assert_eq!(answered, 200);
    }

    #[test]
    fn memory_stays_bounded() {
        let t = Instant::now();
        let mut l = PerSource::new(20.0, 1e9, t);
        for n in 0..5000u32 {
            l.take(IpAddr::from(n.to_be_bytes()), t);
        }
        assert!(l.sources.len() <= MAX_SOURCES);
        // A second later the idle ones make room.
        assert!(l.take(ip(1), t + Duration::from_secs(2)));
    }
}
