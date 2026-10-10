//! Replay protection for packets with counters: accepts each counter once,
//! tolerates reordering within a window (UDP reorders), rejects anything
//! older (as WireGuard and IPsec do).

/// How far back reordered packets are still accepted.
pub const WINDOW: u64 = 2048;

#[derive(Debug)]
pub struct ReplayWindow {
    /// Highest counter accepted, plus one (0 = nothing yet).
    top: u64,
    /// Bit `i` set: counter `top - 1 - i` was seen.
    seen: Vec<u64>,
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self {
            top: 0,
            seen: vec![0; (WINDOW / 64) as usize],
        }
    }
}

impl ReplayWindow {
    fn bit(&self, back: u64) -> bool {
        self.seen[(back / 64) as usize] >> (back % 64) & 1 == 1
    }

    fn set(&mut self, back: u64) {
        self.seen[(back / 64) as usize] |= 1 << (back % 64);
    }

    /// Whether `counter` may be accepted (not seen, not too old). Does not
    /// record it: call [`Self::commit`] once the packet authenticated, so
    /// forged packets cannot move the window.
    pub fn check(&self, counter: u64) -> bool {
        if counter >= self.top {
            return true;
        }
        let back = self.top - 1 - counter;
        back < WINDOW && !self.bit(back)
    }

    pub fn commit(&mut self, counter: u64) {
        if counter >= self.top {
            let shift = counter + 1 - self.top;
            self.shift(shift);
            self.top = counter + 1;
            self.set(0);
        } else {
            let back = self.top - 1 - counter;
            if back < WINDOW {
                self.set(back);
            }
        }
    }

    /// Moves every bit `n` places back (older).
    fn shift(&mut self, n: u64) {
        if n >= WINDOW {
            self.seen.fill(0);
            return;
        }
        let (words, bits) = ((n / 64) as usize, (n % 64) as u32);
        let len = self.seen.len();
        for i in (0..len).rev() {
            let src = i.checked_sub(words);
            let hi = src.map_or(0, |s| self.seen[s] << bits);
            let lo = match (src.and_then(|s| s.checked_sub(1)), bits) {
                (Some(s), b) if b > 0 => self.seen[s] >> (64 - b),
                _ => 0,
            };
            self.seen[i] = hi | lo;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(w: &mut ReplayWindow, c: u64) -> bool {
        let ok = w.check(c);
        if ok {
            w.commit(c);
        }
        ok
    }

    #[test]
    fn each_counter_once() {
        let mut w = ReplayWindow::default();
        assert!(accept(&mut w, 0));
        assert!(!accept(&mut w, 0));
        assert!(accept(&mut w, 1));
        assert!(accept(&mut w, 5));
        assert!(!accept(&mut w, 5));
    }

    #[test]
    fn reordering_within_the_window_is_fine() {
        let mut w = ReplayWindow::default();
        let top = 5000;
        assert!(accept(&mut w, top));
        assert!(accept(&mut w, top - 2));
        assert!(accept(&mut w, top - 1));
        assert!(!accept(&mut w, top - 2));
        assert!(accept(&mut w, top - WINDOW + 1));
        assert!(!accept(&mut w, top - WINDOW), "just outside");
    }

    #[test]
    fn big_jumps_forget_the_old() {
        let mut w = ReplayWindow::default();
        for c in 0..10 {
            assert!(accept(&mut w, c));
        }
        assert!(accept(&mut w, 1_000_000));
        assert!(!accept(&mut w, 5), "far behind now");
        assert!(accept(&mut w, 1_000_000 - 1));
    }

    #[test]
    fn checking_does_not_record() {
        let mut w = ReplayWindow::default();
        assert!(w.check(7));
        assert!(w.check(7), "forged packets must not burn counters");
        w.commit(7);
        assert!(!w.check(7));
    }

    #[test]
    fn shifting_keeps_bits_across_words() {
        let mut w = ReplayWindow::default();
        for c in [0, 63, 64, 65, 130] {
            assert!(accept(&mut w, c));
        }
        for c in [0, 63, 64, 65, 130] {
            assert!(!w.check(c), "{c} was seen");
        }
        for c in [1, 62, 66, 129] {
            assert!(w.check(c), "{c} was not");
        }
    }
}

#[cfg(test)]
mod properties {
    use std::collections::HashSet;

    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// Same answers as the obvious model: accept a counter if it was
        /// never accepted and is within WINDOW of the highest accepted.
        #[test]
        fn matches_a_simple_model(counters in proptest::collection::vec(0u64..6000, 1..400)) {
            let mut w = ReplayWindow::default();
            let mut seen = HashSet::new();
            let mut top: Option<u64> = None;
            for c in counters {
                let model = !seen.contains(&c) && top.is_none_or(|t| c + WINDOW > t);
                prop_assert_eq!(w.check(c), model, "counter {}", c);
                if model {
                    w.commit(c);
                    seen.insert(c);
                    top = Some(top.map_or(c, |t| t.max(c)));
                }
            }
        }
    }
}
