//! The host's pointer: its image arrives in pieces ([`CursorShape`]), its
//! position with every frame ([`Cursor`]). The tracker puts both together
//! for the presenter.

use std::sync::Arc;

use fernsicht_capture::CursorImage;
use fernsicht_proto::{CURSOR_CHUNK, Cursor, CursorShape};
use fernsicht_render::CursorOverlay;

/// An image being put together from its pieces.
struct Assembly {
    serial: u32,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    have: Vec<bool>,
}

#[derive(Default)]
pub(crate) struct CursorTracker {
    assembling: Option<Assembly>,
    /// The newest complete image.
    shape: Option<(u32, Arc<CursorImage>)>,
    position: Option<Cursor>,
    pub(crate) positions: u64,
    pub(crate) shapes: u64,
}

impl CursorTracker {
    pub(crate) fn on_shape(&mut self, s: &CursorShape, data: &[u8]) {
        if self
            .shape
            .as_ref()
            .is_some_and(|(serial, _)| *serial == s.serial)
        {
            return; // a repeat of what we have
        }
        let (w, h) = (u32::from(s.width), u32::from(s.height));
        let fresh = self
            .assembling
            .as_ref()
            .is_none_or(|a| (a.serial, a.width, a.height) != (s.serial, w, h));
        if fresh {
            let len = s.image_len();
            self.assembling = Some(Assembly {
                serial: s.serial,
                width: w,
                height: h,
                pixels: vec![0; len],
                have: vec![false; len.div_ceil(CURSOR_CHUNK)],
            });
        }
        let a = self.assembling.as_mut().expect("just set");
        // The parser checked offset and length against the image size.
        let offset = s.offset as usize;
        a.pixels[offset..offset + data.len()].copy_from_slice(data);
        a.have[offset / CURSOR_CHUNK] = true;
        if a.have.iter().all(|&h| h) {
            let a = self.assembling.take().expect("checked");
            self.shapes += 1;
            self.shape = Some((
                a.serial,
                Arc::new(CursorImage {
                    width: a.width,
                    height: a.height,
                    pixels: a.pixels,
                }),
            ));
        }
    }

    pub(crate) fn on_position(&mut self, c: Cursor) {
        self.positions += 1;
        self.position = Some(c);
    }

    /// What to draw. Until the image of a new shape is complete, the
    /// previous one stands in.
    pub(crate) fn overlay(&self) -> Option<CursorOverlay> {
        let c = self.position?;
        let (serial, image) = self.shape.as_ref()?;
        c.visible.then(|| CursorOverlay {
            serial: *serial,
            image: image.clone(),
            x: c.x,
            y: c.y,
            screen_width: u32::from(c.screen_width),
            screen_height: u32::from(c.screen_height),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(serial: u32, w: u16, h: u16, offset: usize) -> CursorShape {
        CursorShape {
            session_id: 1,
            serial,
            width: w,
            height: h,
            offset: offset as u32,
        }
    }

    fn image(w: u16, h: u16, seed: u8) -> Vec<u8> {
        (0..usize::from(w) * usize::from(h) * 4)
            .map(|i| (i as u8).wrapping_add(seed))
            .collect()
    }

    fn feed(t: &mut CursorTracker, serial: u32, w: u16, h: u16, img: &[u8], skip: Option<usize>) {
        for (i, chunk) in img.chunks(CURSOR_CHUNK).enumerate() {
            if Some(i) != skip {
                t.on_shape(&shape(serial, w, h, i * CURSOR_CHUNK), chunk);
            }
        }
    }

    fn at(x: i32, y: i32, serial: u32, visible: bool) -> Cursor {
        Cursor {
            session_id: 1,
            visible,
            shape_serial: serial,
            x,
            y,
            screen_width: 2560,
            screen_height: 1440,
        }
    }

    #[test]
    fn pieces_in_any_order_make_the_image() {
        let mut t = CursorTracker::default();
        let img = image(32, 32, 0); // 4096 bytes, 4 pieces
        let chunks: Vec<&[u8]> = img.chunks(CURSOR_CHUNK).collect();
        for i in [2, 0, 3] {
            t.on_shape(&shape(1, 32, 32, i * CURSOR_CHUNK), chunks[i]);
        }
        t.on_position(at(10, 20, 1, true));
        assert_eq!(t.overlay(), None, "a piece is missing");
        t.on_shape(&shape(1, 32, 32, CURSOR_CHUNK), chunks[1]);
        let o = t.overlay().unwrap();
        assert_eq!(o.image.pixels, img);
        assert_eq!((o.x, o.y, o.screen_width, o.serial), (10, 20, 2560, 1));
        assert_eq!(t.shapes, 1);
    }

    #[test]
    fn a_lost_piece_heals_with_the_repeat() {
        let mut t = CursorTracker::default();
        let img = image(24, 24, 7);
        t.on_position(at(0, 0, 3, true));
        feed(&mut t, 3, 24, 24, &img, Some(1));
        assert_eq!(t.overlay(), None);
        feed(&mut t, 3, 24, 24, &img, None);
        assert_eq!(t.overlay().unwrap().image.pixels, img);
        // Further repeats change nothing.
        feed(&mut t, 3, 24, 24, &img, None);
        assert_eq!(t.shapes, 1);
    }

    #[test]
    fn the_old_shape_stands_in_until_the_new_one_is_complete() {
        let mut t = CursorTracker::default();
        let (arrow, hand) = (image(16, 16, 1), image(20, 20, 2));
        feed(&mut t, 1, 16, 16, &arrow, None);
        t.on_position(at(5, 5, 2, true));
        feed(&mut t, 2, 20, 20, &hand, Some(0));
        let o = t.overlay().unwrap();
        assert_eq!((o.serial, o.image.width), (1, 16));
        feed(&mut t, 2, 20, 20, &hand, None);
        assert_eq!(t.overlay().unwrap().serial, 2);
    }

    #[test]
    fn hidden_pointer_is_not_drawn() {
        let mut t = CursorTracker::default();
        feed(&mut t, 1, 8, 8, &image(8, 8, 0), None);
        t.on_position(at(1, 1, 1, false));
        assert_eq!(t.overlay(), None);
        t.on_position(at(1, 1, 1, true));
        assert!(t.overlay().is_some());
        assert_eq!(t.positions, 2);
    }

    #[test]
    fn a_new_shape_of_another_size_restarts_the_assembly() {
        let mut t = CursorTracker::default();
        // Half of a 32×32 image, then a complete 8×8 one with the same serial
        // (a host restart): the 8×8 wins, no mix of the two.
        let big = image(32, 32, 0);
        t.on_shape(&shape(4, 32, 32, 0), &big[..CURSOR_CHUNK]);
        let small = image(8, 8, 9);
        feed(&mut t, 4, 8, 8, &small, None);
        t.on_position(at(0, 0, 4, true));
        assert_eq!(t.overlay().unwrap().image.pixels, small);
    }
}
