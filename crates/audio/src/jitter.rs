//! The client's jitter buffer: frames arrive with gaps, duplicates (every
//! packet repeats the previous frame) and out of order; playback takes
//! one frame every 5 ms. It holds them in fixed slots: nothing allocates
//! per frame.

use fernsicht_proto::MAX_AUDIO_FRAME;

/// What playback gets next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next<'a> {
    /// The frame's encoded bytes.
    Frame(&'a [u8]),
    /// This frame is missing but later ones are here: conceal it.
    Lost,
    /// Nothing to play (not started, or ran dry): conceal or play silence.
    Empty,
}

/// An encoded frame in fixed memory (at most [`MAX_AUDIO_FRAME`] bytes).
#[derive(Clone, Copy)]
pub struct Encoded {
    len: u16,
    bytes: [u8; MAX_AUDIO_FRAME],
}

impl Encoded {
    /// `None` when `data` is longer than a frame may be.
    pub fn new(data: &[u8]) -> Option<Self> {
        let mut bytes = [0u8; MAX_AUDIO_FRAME];
        bytes.get_mut(..data.len())?.copy_from_slice(data);
        Some(Self {
            len: data.len() as u16,
            bytes,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl std::fmt::Debug for Encoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Encoded({} bytes)", self.len)
    }
}

/// Seq numbers are 32 bit and wrap; compare by distance.
fn after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Slots, a power of two so that a seq number keeps its slot across the
/// wrap; more than the bound below for any target in use.
const SLOTS: usize = 32;

#[derive(Clone, Copy, Debug)]
struct Slot {
    seq: u32,
    full: bool,
    frame: Encoded,
}

#[derive(Debug)]
pub struct Jitter {
    /// Frames to hold before playing starts (and after running dry).
    target: usize,
    /// Frame `seq` lives in slot `seq % SLOTS`.
    slots: Box<[Slot; SLOTS]>,
    /// Slots in use.
    held: usize,
    /// The next frame to play, once started.
    next: Option<u32>,
    pub played: u64,
    pub concealed: u64,
    pub dropped: u64,
}

impl Jitter {
    pub fn new(target: usize) -> Self {
        let target = target.clamp(1, SLOTS / 4 - 1);
        let empty = Slot {
            seq: 0,
            full: false,
            frame: Encoded {
                len: 0,
                bytes: [0; MAX_AUDIO_FRAME],
            },
        };
        Self {
            target,
            slots: Box::new([empty; SLOTS]),
            held: 0,
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
        let Some(frame) = Encoded::new(data) else {
            return;
        };
        let slot = &mut self.slots[seq as usize % SLOTS];
        if slot.full {
            if slot.seq == seq {
                return; // the redundant copy
            }
            // A frame a whole ring older: long due.
            self.held -= 1;
            self.dropped += 1;
        }
        *slot = Slot {
            seq,
            full: true,
            frame,
        };
        self.held += 1;
        // A burst after a stall would build up delay: keep at most a
        // generous buffer, drop the oldest.
        while self.held > self.target * 4 {
            let oldest = self.oldest().expect("non-empty");
            self.slots[oldest as usize % SLOTS].full = false;
            self.held -= 1;
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

    /// The earliest frame held, in sequence order (across the wrap).
    fn oldest(&self) -> Option<u32> {
        self.slots
            .iter()
            .filter(|s| s.full)
            .map(|s| s.seq)
            .reduce(|best, k| if after(best, k) { k } else { best })
    }

    /// Frames waiting from the next one on.
    pub fn depth(&self) -> usize {
        self.held
    }

    pub fn pop(&mut self) -> Next<'_> {
        let next = match self.next {
            Some(n) => n,
            None => {
                if self.held < self.target {
                    return Next::Empty;
                }
                self.oldest().expect("non-empty")
            }
        };
        let i = next as usize % SLOTS;
        if self.slots[i].full && self.slots[i].seq == next {
            self.slots[i].full = false;
            self.held -= 1;
            self.next = Some(next.wrapping_add(1));
            self.played += 1;
            return Next::Frame(self.slots[i].frame.as_slice());
        }
        if self.held == 0 {
            // Ran dry: wait for `target` frames again (the delay grows by
            // the gap instead of the sound stuttering frame by frame).
            self.next = None;
            return Next::Empty;
        }
        self.next = Some(next.wrapping_add(1));
        self.concealed += 1;
        Next::Lost
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
        assert_eq!(j.pop(), Next::Frame(&f(10)));
        assert_eq!(j.pop(), Next::Frame(&f(11)));
        assert_eq!(j.pop(), Next::Frame(&f(12)));
        assert_eq!(j.played, 3);
    }

    #[test]
    fn duplicates_and_late_frames_are_ignored() {
        let mut j = Jitter::new(1);
        j.push(1, &f(1));
        j.push(1, &f(1));
        assert_eq!(j.pop(), Next::Frame(&f(1)));
        j.push(1, &f(1)); // the redundant copy in the next packet
        j.push(0, &f(0)); // older than what was played
        j.push(2, &f(2));
        assert_eq!(j.pop(), Next::Frame(&f(2)));
        assert_eq!(j.depth(), 0);
    }

    #[test]
    fn a_gap_is_concealed_when_later_frames_wait() {
        let mut j = Jitter::new(2);
        j.push(1, &f(1));
        j.push(3, &f(3));
        assert_eq!(j.pop(), Next::Frame(&f(1)));
        assert_eq!(j.pop(), Next::Lost);
        assert_eq!(j.pop(), Next::Frame(&f(3)));
        assert_eq!(j.concealed, 1);
    }

    #[test]
    fn the_redundant_copy_fills_a_gap_in_time() {
        let mut j = Jitter::new(2);
        j.push(1, &f(1));
        j.push(3, &f(3)); // packet 2 lost …
        assert_eq!(j.pop(), Next::Frame(&f(1)));
        j.push(2, &f(2)); // … but packet 3 carried it again
        assert_eq!(j.pop(), Next::Frame(&f(2)));
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
        assert_eq!(j.pop(), Next::Frame(&f(5)));
    }

    #[test]
    fn delay_is_bounded() {
        let mut j = Jitter::new(2);
        for s in 0..20 {
            j.push(s, &f(s));
        }
        assert_eq!(j.depth(), 8, "at most four times the target");
        assert_eq!(j.dropped, 12);
        assert_eq!(j.pop(), Next::Frame(&f(12)));
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut j = Jitter::new(2);
        j.push(0, &f(2));
        j.push(u32::MAX, &f(1));
        assert_eq!(
            j.pop(),
            Next::Frame(&f(1)),
            "the older one, across the wrap"
        );
        assert_eq!(j.pop(), Next::Frame(&f(2)));
        // Bounding drops the oldest across the wrap, too.
        let mut j = Jitter::new(1);
        for s in [u32::MAX - 1, u32::MAX, 0, 1, 2] {
            j.push(s, &f(s));
        }
        assert_eq!(j.depth(), 4);
        assert_eq!(j.pop(), Next::Frame(&f(u32::MAX)));
        assert_eq!(j.pop(), Next::Frame(&f(0)));
    }
}
