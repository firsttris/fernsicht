//! A clipboard text across a lossy link: cut into chunks that fit a
//! datagram, acknowledged by how far the receiver has it without gaps, and
//! sent again from there until it is all acknowledged (go-back-N). Texts
//! are small and rare, so this is simple rather than fast. A newer text
//! (another serial) replaces one still on its way, on both sides.

/// Bytes of text one packet carries.
pub const CHUNK: usize = fernsicht_proto::CLIPBOARD_CHUNK;

/// The longest text that goes across (larger ones stay where they are).
pub const MAX_TEXT: usize = fernsicht_proto::MAX_CLIPBOARD_TEXT;

/// A text being sent.
#[derive(Debug)]
pub struct Outgoing {
    serial: u32,
    text: Vec<u8>,
    /// Bytes the receiver has without gaps.
    acked: usize,
    /// When it was last sent (from `acked` on), µs.
    sent_at: Option<u64>,
}

impl Outgoing {
    /// `None` for a text over [`MAX_TEXT`].
    pub fn new(serial: u32, text: &str) -> Option<Self> {
        (text.len() <= MAX_TEXT).then(|| Self {
            serial,
            text: text.as_bytes().to_vec(),
            acked: 0,
            sent_at: None,
        })
    }

    pub fn serial(&self) -> u32 {
        self.serial
    }

    pub fn total(&self) -> u32 {
        self.text.len() as u32
    }

    /// The receiver has `upto` bytes of text `serial`.
    pub fn ack(&mut self, serial: u32, upto: u32) {
        if serial == self.serial {
            self.acked = self.acked.max((upto as usize).min(self.text.len()));
        }
    }

    pub fn done(&self) -> bool {
        self.acked >= self.text.len() && self.sent_at.is_some()
    }

    /// The chunks to send now, as (offset, bytes): all of them the first
    /// time, then from what is acknowledged on once `resend_us` passed
    /// without the rest being acknowledged. An empty text is one empty
    /// chunk.
    pub fn due(&mut self, now_us: u64, resend_us: u64) -> Vec<(u32, &[u8])> {
        let waiting = self
            .sent_at
            .is_some_and(|t| now_us.saturating_sub(t) < resend_us);
        if self.done() || waiting {
            return Vec::new();
        }
        self.sent_at = Some(now_us);
        if self.text.is_empty() {
            return vec![(0, &[][..])];
        }
        let from = self.acked - self.acked % CHUNK;
        self.text[from..]
            .chunks(CHUNK)
            .enumerate()
            .map(|(i, c)| ((from + i * CHUNK) as u32, c))
            .collect()
    }
}

/// What a received chunk means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Received {
    /// Not (or no longer) wanted: an older text, or a broken chunk.
    Ignored,
    /// The receiver has `upto` bytes without gaps: acknowledge that.
    Partial { upto: u32 },
    /// The whole text arrived (acknowledge `upto` = its length).
    Complete { text: String, upto: u32 },
}

/// A text being received.
#[derive(Debug, Default)]
pub struct Incoming {
    serial: Option<u32>,
    total: usize,
    buf: Vec<u8>,
    have: Vec<bool>,
    /// The last text completed (repeated chunks of it are acknowledged
    /// again, not delivered again).
    done: Option<u32>,
}

impl Incoming {
    /// One chunk of text `serial` (`total` bytes, these at `offset`).
    pub fn push(&mut self, serial: u32, total: u32, offset: u32, data: &[u8]) -> Received {
        let (total, offset) = (total as usize, offset as usize);
        let fits = total <= MAX_TEXT
            && data.len() <= CHUNK
            && offset.is_multiple_of(CHUNK)
            && offset + data.len() <= total
            && (data.len() == CHUNK.min(total - offset) || total == 0);
        if !fits {
            return Received::Ignored;
        }
        if self.done == Some(serial) {
            return Received::Partial { upto: total as u32 };
        }
        // A newer text replaces one being received; an older one is late.
        if self.serial != Some(serial) {
            if self.serial.is_some_and(|s| !newer(serial, s)) {
                return Received::Ignored;
            }
            self.serial = Some(serial);
            self.total = total;
            self.buf = vec![0; total];
            self.have = vec![false; total.div_ceil(CHUNK)];
        } else if total != self.total {
            return Received::Ignored;
        }
        if total > 0 {
            self.buf[offset..offset + data.len()].copy_from_slice(data);
            self.have[offset / CHUNK] = true;
        }
        let upto = (self.have.iter().take_while(|&&h| h).count() * CHUNK).min(total) as u32;
        if upto as usize == total {
            let text = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            // The serial stays: older texts arriving late are ignored.
            self.done = Some(serial);
            return Received::Complete { text, upto };
        }
        Received::Partial { upto }
    }
}

/// One session's clipboard, both ways: the text being sent, the one being
/// received, and the text last exchanged, which is not sent back (a
/// desktop announces its clipboard again now and then).
#[derive(Debug, Default)]
pub struct Sync {
    serial: u32,
    out: Option<Outgoing>,
    inc: Incoming,
    last: Option<String>,
}

/// What a received piece asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reply {
    /// Acknowledge this much of text `.0` (`.1` bytes).
    pub ack: Option<(u32, u32)>,
    /// Put this on the local clipboard: the whole text arrived.
    pub text: Option<String>,
}

impl Sync {
    /// The local clipboard now holds `text`: it goes across, unless it is
    /// the text last exchanged or too long. Whether it goes.
    pub fn copied(&mut self, text: String) -> bool {
        if self.last.as_deref() == Some(&text) {
            return false;
        }
        self.serial = self.serial.wrapping_add(1);
        let Some(out) = Outgoing::new(self.serial, &text) else {
            return false;
        };
        self.out = Some(out);
        self.last = Some(text);
        true
    }

    /// The pieces to send now: (serial, total, offset, bytes).
    pub fn due(&mut self, now_us: u64, resend_us: u64) -> Vec<(u32, u32, u32, &[u8])> {
        let Some(out) = self.out.as_mut() else {
            return Vec::new();
        };
        let (serial, total) = (out.serial(), out.total());
        out.due(now_us, resend_us)
            .into_iter()
            .map(|(offset, data)| (serial, total, offset, data))
            .collect()
    }

    /// The other side has `upto` bytes of our text `serial`.
    pub fn acked(&mut self, serial: u32, upto: u32) {
        if let Some(out) = self.out.as_mut() {
            out.ack(serial, upto);
        }
    }

    /// A piece of the other side's text.
    pub fn piece(&mut self, serial: u32, total: u32, offset: u32, data: &[u8]) -> Reply {
        match self.inc.push(serial, total, offset, data) {
            Received::Ignored => Reply::default(),
            Received::Partial { upto } => Reply {
                ack: Some((serial, upto)),
                text: None,
            },
            Received::Complete { text, upto } => {
                self.last = Some(text.clone());
                Reply {
                    ack: Some((serial, upto)),
                    text: Some(text),
                }
            }
        }
    }
}

/// `a` came after `b` (serials wrap).
pub fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sends `text` through a link that drops every chunk `lose` picks,
    /// resending until done; returns what arrived and the rounds it took.
    fn across(text: &str, mut lose: impl FnMut(usize) -> bool) -> (Option<String>, u32) {
        let mut out = Outgoing::new(7, text).unwrap();
        let mut inc = Incoming::default();
        let (mut got, mut n, mut rounds) = (None, 0, 0);
        let mut now = 0;
        while !out.done() && rounds < 1000 {
            rounds += 1;
            now += 300_000;
            let (serial, total) = (out.serial(), out.total());
            let sent: Vec<(u32, Vec<u8>)> = out
                .due(now, 200_000)
                .into_iter()
                .map(|(o, d)| (o, d.to_vec()))
                .collect();
            for (offset, data) in sent {
                n += 1;
                if lose(n) {
                    continue;
                }
                match inc.push(serial, total, offset, &data) {
                    Received::Partial { upto } => out.ack(serial, upto),
                    Received::Complete { text, upto } => {
                        got = Some(text);
                        out.ack(serial, upto);
                    }
                    Received::Ignored => {}
                }
            }
        }
        (got, rounds)
    }

    #[test]
    fn a_text_arrives_whole_through_loss() {
        let text = "Grüße, ".repeat(2_000);
        assert_eq!(across(&text, |_| false), (Some(text.clone()), 1));
        let (got, rounds) = across(&text, |n| n % 3 == 0);
        assert_eq!(got.as_deref(), Some(text.as_str()));
        assert!(rounds > 1, "lost chunks went again");
        assert_eq!(
            across("", |_| false).0.as_deref(),
            Some(""),
            "an emptied clipboard"
        );
    }

    #[test]
    fn too_long_and_broken_texts_stay_out() {
        assert!(Outgoing::new(1, &"x".repeat(MAX_TEXT + 1)).is_none());
        let mut inc = Incoming::default();
        assert_eq!(
            inc.push(1, 10, 3, b"abc"),
            Received::Ignored,
            "offset off the grid"
        );
        assert_eq!(
            inc.push(1, 10, 0, b"abc"),
            Received::Ignored,
            "short of its chunk"
        );
        assert_eq!(
            inc.push(1, MAX_TEXT as u32 + 1, 0, &[0; CHUNK]),
            Received::Ignored,
            "too long"
        );
        assert_eq!(
            inc.push(1, 4, 1024, b"abcd"),
            Received::Ignored,
            "past its end"
        );
    }

    #[test]
    fn a_session_does_not_send_back_what_it_got() {
        let (mut host, mut client) = (Sync::default(), Sync::default());
        assert!(client.copied("vom Client".into()));
        let mut set_on_host = None;
        let pieces: Vec<_> = client
            .due(0, 200_000)
            .into_iter()
            .map(|(s, t, o, d)| (s, t, o, d.to_vec()))
            .collect();
        for (s, t, o, d) in pieces {
            let r = host.piece(s, t, o, &d);
            if let Some((serial, upto)) = r.ack {
                client.acked(serial, upto);
            }
            set_on_host = r.text.or(set_on_host);
        }
        assert_eq!(set_on_host.as_deref(), Some("vom Client"));
        assert!(client.due(1_000_000, 200_000).is_empty(), "acknowledged");
        // The host's desktop announces the text it was given: not sent back.
        assert!(!host.copied("vom Client".into()));
        assert!(host.due(0, 200_000).is_empty());
        // Copied again on the client after something else: it goes.
        assert!(client.copied("anders".into()));
        assert!(!client.copied("anders".into()), "the same text twice");
    }

    #[test]
    fn a_newer_text_replaces_one_on_its_way() {
        let mut inc = Incoming::default();
        let long = vec![b'a'; 3 * CHUNK];
        assert_eq!(
            inc.push(1, long.len() as u32, 0, &long[..CHUNK]),
            Received::Partial { upto: 1024 }
        );
        assert_eq!(
            inc.push(2, 2, 0, b"hi"),
            Received::Complete {
                text: "hi".into(),
                upto: 2
            }
        );
        assert_eq!(
            inc.push(1, long.len() as u32, CHUNK as u32, &long[..CHUNK]),
            Received::Ignored,
            "the older text is late"
        );
        // A chunk of the text already delivered is acknowledged again,
        // not delivered twice.
        assert_eq!(inc.push(2, 2, 0, b"hi"), Received::Partial { upto: 2 });
        assert!(newer(0, u32::MAX), "serials wrap");
    }
}
