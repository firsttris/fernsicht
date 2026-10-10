//! The client's jitter buffer: frames arrive with gaps, duplicates (every
//! packet repeats the previous frame) and out of order; playback takes
//! one frame every 5 ms.

use std::collections::BTreeMap;

/// What playback gets next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// The frame's encoded bytes.
    Frame(Vec<u8>),
    /// This frame is missing but later ones are here: conceal it.
    Lost,
    /// Nothing to play (not started, or ran dry): conceal or play silence.
    Empty,
}

/// Seq numbers are 32 bit and wrap; compare by distance.
fn after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// The earliest of `keys` in sequence order (across the wrap).
fn oldest<'a>(keys: impl Iterator<Item = &'a u32>) -> Option<u32> {
    keys.copied()
        .reduce(|best, k| if after(best, k) { k } else { best })
}

#[derive(Debug)]
pub struct Jitter {
    /// Frames to hold before playing starts (and after running dry).
    target: usize,
    frames: BTreeMap<u32, Vec<u8>>,
    /// The next frame to play, once started.
    next: Option<u32>,
    pub played: u64,
    pub concealed: u64,
    pub dropped: u64,
}

impl Jitter {
    pub fn new(target: usize) -> Self {
        Self {
            target: target.max(1),
            frames: BTreeMap::new(),
            next: None,
            played: 0,
            concealed: 0,
            dropped: 0,
        }
    }

    pub fn push(&mut self, seq: u32, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if self.next.is_some_and(|n| !after(seq, n) && seq != n) {
            return; // already played or skipped
        }
        self.frames.entry(seq).or_insert_with(|| data.to_vec());
        // A burst after a stall would build up delay: keep at most a
        // generous buffer, drop the oldest.
        while self.frames.len() > self.target * 4 {
            let oldest = oldest(self.frames.keys()).expect("non-empty");
            self.frames.remove(&oldest);
            self.dropped += 1;
            self.next = self.next.map(|n| {
                if n == oldest {
                    oldest.wrapping_add(1)
                } else {
                    n
                }
            });
        }
    }

    /// Frames waiting from the next one on.
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    pub fn pop(&mut self) -> Next {
        let next = match self.next {
            Some(n) => n,
            None => {
                if self.frames.len() < self.target {
                    return Next::Empty;
                }
                oldest(self.frames.keys()).expect("non-empty")
            }
        };
        if let Some(data) = self.frames.remove(&next) {
            self.next = Some(next.wrapping_add(1));
            self.played += 1;
            return Next::Frame(data);
        }
        if self.frames.is_empty() {
            // Ran dry: wait for `target` frames again (the delay grows by
            // the gap instead of the sound stuttering frame by frame).
            self.next = None;
            return Next::Empty;
        }
        self.next = Some(next.wrapping_add(1));
        self.concealed += 1;
        Next::Lost
    }

    /// Drops the oldest waiting frame, to shorten the delay when the
    /// buffer grew (sound card clock slower than the host's).
    pub fn skip_one(&mut self) {
        if let Some(seq) = oldest(self.frames.keys()) {
            self.frames.remove(&seq);
            self.dropped += 1;
            if self.next == Some(seq) {
                self.next = Some(seq.wrapping_add(1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(seq: u32) -> Vec<u8> {
        seq.to_le_bytes().to_vec()
    }

    #[test]
    fn waits_for_the_target_then_plays_in_order() {
        let mut j = Jitter::new(3);
        j.push(10, &f(10));
        j.push(12, &f(12));
        assert_eq!(j.pop(), Next::Empty, "only two frames");
        j.push(11, &f(11));
        assert_eq!(j.pop(), Next::Frame(f(10)));
        assert_eq!(j.pop(), Next::Frame(f(11)));
        assert_eq!(j.pop(), Next::Frame(f(12)));
        assert_eq!(j.played, 3);
    }

    #[test]
    fn duplicates_and_late_frames_are_ignored() {
        let mut j = Jitter::new(1);
        j.push(1, &f(1));
        j.push(1, &f(1));
        assert_eq!(j.pop(), Next::Frame(f(1)));
        j.push(1, &f(1)); // the redundant copy in the next packet
        j.push(0, &f(0)); // older than what was played
        j.push(2, &f(2));
        assert_eq!(j.pop(), Next::Frame(f(2)));
        assert_eq!(j.depth(), 0);
    }

    #[test]
    fn a_gap_is_concealed_when_later_frames_wait() {
        let mut j = Jitter::new(2);
        j.push(1, &f(1));
        j.push(3, &f(3));
        assert_eq!(j.pop(), Next::Frame(f(1)));
        assert_eq!(j.pop(), Next::Lost);
        assert_eq!(j.pop(), Next::Frame(f(3)));
        assert_eq!(j.concealed, 1);
    }

    #[test]
    fn the_redundant_copy_fills_a_gap_in_time() {
        let mut j = Jitter::new(2);
        j.push(1, &f(1));
        j.push(3, &f(3)); // packet 2 lost …
        assert_eq!(j.pop(), Next::Frame(f(1)));
        j.push(2, &f(2)); // … but packet 3 carried it again
        assert_eq!(j.pop(), Next::Frame(f(2)));
        assert_eq!(j.concealed, 0);
    }

    #[test]
    fn running_dry_waits_for_the_target_again() {
        let mut j = Jitter::new(2);
        j.push(1, &f(1));
        j.push(2, &f(2));
        j.pop();
        j.pop();
        assert_eq!(j.pop(), Next::Empty);
        j.push(5, &f(5));
        assert_eq!(j.pop(), Next::Empty, "one frame is not enough");
        j.push(6, &f(6));
        assert_eq!(j.pop(), Next::Frame(f(5)));
    }

    #[test]
    fn delay_is_bounded_and_can_be_shortened() {
        let mut j = Jitter::new(2);
        for s in 0..20 {
            j.push(s, &f(s));
        }
        assert_eq!(j.depth(), 8, "at most four times the target");
        assert_eq!(j.dropped, 12);
        assert_eq!(j.pop(), Next::Frame(f(12)));
        j.skip_one();
        assert_eq!(j.pop(), Next::Frame(f(14)));
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut j = Jitter::new(2);
        j.push(0, &f(2));
        j.push(u32::MAX, &f(1));
        assert_eq!(j.pop(), Next::Frame(f(1)), "the older one, across the wrap");
        assert_eq!(j.pop(), Next::Frame(f(2)));
        // Bounding and skipping drop the oldest across the wrap, too.
        let mut j = Jitter::new(1);
        for s in [u32::MAX - 1, u32::MAX, 0, 1, 2] {
            j.push(s, &f(s));
        }
        assert_eq!(j.depth(), 4);
        assert_eq!(j.pop(), Next::Frame(f(u32::MAX)));
        j.skip_one();
        assert_eq!(j.pop(), Next::Frame(f(1)));
    }
}
