//! The pointer: its position with every frame and its image when it
//! changes, repeated so that losses heal.

use std::time::{Duration, Instant};

use fernsicht_capture::CursorState;
use fernsicht_proto::{CURSOR_CHUNK, Cursor, CursorShape, MAX_CURSOR_SIZE, MAX_DATAGRAM};

use crate::link::Link;

/// An unchanged cursor image is sent again, so that a lost piece heals:
/// soon after a change (the first copy of a session can even arrive
/// before the client knows the session), then less often, up to every 2 s.
const CURSOR_SHAPE_FIRST_REPEAT: Duration = Duration::from_millis(100);
const CURSOR_SHAPE_REPEAT: Duration = Duration::from_secs(2);

/// The pointer's image is read against where input put it once the pointer
/// rested this long (the compositor has drawn it by then), and not after
/// this long (something else may have moved it since).
const SETTLED_MS: std::ops::Range<u32> = 50..2_000;

/// Hotspots learnt, one per image (the last few shapes).
const HOTSPOTS: usize = 16;

/// Where in the pointer's image the pointer is (its hotspot, which KMS does
/// not tell): input put the pointer at `at` (0..65535 on the monitor shown,
/// `screen` pixels), and the image of `size` sits at `image_at`. `None`
/// when that point is not inside the image (the pointer moved otherwise).
pub(crate) fn hotspot(
    at: (u16, u16),
    screen: (u16, u16),
    image_at: (i32, i32),
    size: (u32, u32),
) -> Option<(u16, u16)> {
    let pixel = |v: u16, len: u16| (u32::from(v) * u32::from(len) / 65_536) as i32;
    let dx = pixel(at.0, screen.0) - image_at.0;
    let dy = pixel(at.1, screen.1) - image_at.1;
    let inside = (0..size.0 as i32).contains(&dx) && (0..size.1 as i32).contains(&dy);
    inside.then_some((dx as u16, dy as u16))
}

/// Sends the pointer next to the video: its position with every captured
/// frame, its image when it changes and every [`CURSOR_SHAPE_REPEAT`].
pub(crate) struct CursorSender {
    link: Link,
    session_id: u32,
    pub(crate) screen: (u16, u16),
    /// Serial of the image last sent, when, and the wait for the repeat.
    sent: Option<(u32, Instant, Duration)>,
    warned: bool,
    /// Hotspots learnt, by image serial, the newest last.
    hotspots: Vec<(u32, (u16, u16))>,
}

impl CursorSender {
    pub(crate) fn new(link: Link, session_id: u32, screen: (u32, u32)) -> Self {
        let mut s = Self {
            link,
            session_id,
            screen: (0, 0),
            sent: None,
            warned: false,
            hotspots: Vec::new(),
        };
        s.set_screen(screen);
        s
    }

    /// The size of the monitor shown (pointer positions are relative to it).
    pub(crate) fn set_screen(&mut self, screen: (u32, u32)) {
        let clamp = |v: u32| v.min(u32::from(u16::MAX)) as u16;
        self.screen = (clamp(screen.0), clamp(screen.1));
    }

    /// `asked`: the client said it lacks the image; send it now. `pointer`:
    /// where input last put the pointer, and how many ms ago.
    pub(crate) fn send(
        &mut self,
        c: &CursorState,
        asked: bool,
        pointer: Option<((u16, u16), u32)>,
    ) {
        let mut buf = [0u8; MAX_DATAGRAM];
        let img = &c.image;
        let fits = (1..=u32::from(MAX_CURSOR_SIZE)).contains(&img.width)
            && (1..=u32::from(MAX_CURSOR_SIZE)).contains(&img.height)
            && img.pixels.len() == (img.width * img.height * 4) as usize;
        if !fits {
            if !self.warned {
                log::warn!("pointer image {}×{} cannot be sent", img.width, img.height);
                self.warned = true;
            }
            return;
        }
        let repeat = match self.sent {
            // Asked for: right away (feedback comes every 100 ms at most).
            Some((serial, _, wait)) if serial == c.serial && asked => Some(wait),
            Some((serial, at, wait)) if serial == c.serial => {
                (at.elapsed() >= wait).then(|| (wait * 2).min(CURSOR_SHAPE_REPEAT))
            }
            _ => Some(CURSOR_SHAPE_FIRST_REPEAT),
        };
        if let Some(next_wait) = repeat {
            for (i, chunk) in img.pixels.chunks(CURSOR_CHUNK).enumerate() {
                let n = CursorShape {
                    session_id: self.session_id,
                    serial: c.serial,
                    width: img.width as u16,
                    height: img.height as u16,
                    offset: (i * CURSOR_CHUNK) as u32,
                }
                .encode(chunk, &mut buf);
                let _ = self.link.send(&buf[..n]);
            }
            self.sent = Some((c.serial, Instant::now(), next_wait));
        }
        if let Some((at, age)) = pointer
            && c.visible
            && SETTLED_MS.contains(&age)
            && let Some(h) = hotspot(at, self.screen, (c.x, c.y), (img.width, img.height))
        {
            self.hotspots.retain(|(serial, _)| *serial != c.serial);
            if self.hotspots.len() == HOTSPOTS {
                self.hotspots.remove(0);
            }
            self.hotspots.push((c.serial, h));
        }
        let hotspot = self
            .hotspots
            .iter()
            .find(|(serial, _)| *serial == c.serial)
            .map(|(_, h)| *h);
        let n = Cursor {
            session_id: self.session_id,
            visible: c.visible,
            shape_serial: c.serial,
            x: c.x,
            y: c.y,
            screen_width: self.screen.0,
            screen_height: self.screen.1,
            hotspot,
        }
        .encode(&mut buf);
        let _ = self.link.send(&buf[..n]);
    }
}
