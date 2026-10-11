//! The host's pointer: its image arrives in pieces ([`CursorShape`]), its
//! position with every frame ([`Cursor`]). The tracker puts both together
//! for the presenter. Once the host learnt where in the image the pointer
//! is (its hotspot), the window can show the image as this machine's own
//! pointer, which moves without any delay ([`PointerShare::local`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
            log::debug!("pointer image {} ({}×{})", a.serial, a.width, a.height);
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

    /// The host announced a pointer image we do not have (complete).
    pub(crate) fn needs_shape(&self) -> bool {
        self.position.is_some_and(|c| {
            c.shape_serial != 0
                && self
                    .shape
                    .as_ref()
                    .is_none_or(|(s, _)| *s != c.shape_serial)
        })
    }

    /// The host's pointer for this machine to show as its own: visible,
    /// complete, and its hotspot known.
    pub(crate) fn local(&self) -> Option<LocalPointer> {
        let c = self.position?;
        let (serial, image) = self.shape.as_ref()?;
        let hotspot = c.hotspot?;
        (c.visible && *serial == c.shape_serial).then(|| LocalPointer {
            serial: *serial,
            image: image.clone(),
            hotspot,
            screen: (u32::from(c.screen_width), u32::from(c.screen_height)),
        })
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

/// The host's pointer as this machine's own.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalPointer {
    /// Changes with the image.
    pub serial: u32,
    /// BGRA, premultiplied alpha, in the host's screen pixels.
    pub image: Arc<CursorImage>,
    /// Where in the image the pointer is.
    pub hotspot: (u16, u16),
    /// The host's screen size: the picture may be scaled from it.
    pub screen: (u32, u32),
}

/// The pointer, shared between the network thread (fills it), the present
/// thread (draws it into the picture) and the window (shows it as its own
/// pointer instead, when it can).
#[derive(Default)]
pub struct PointerShare {
    pub(crate) tracker: Mutex<CursorTracker>,
    /// The window shows the pointer itself: not drawn into the picture.
    shown_locally: AtomicBool,
}

impl std::fmt::Debug for PointerShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PointerShare").finish_non_exhaustive()
    }
}

impl PointerShare {
    /// The host's pointer for this machine to show as its own, if it can.
    pub fn local(&self) -> Option<LocalPointer> {
        self.tracker.lock().ok()?.local()
    }

    /// Whether the window shows the pointer itself now.
    pub fn set_shown_locally(&self, on: bool) {
        self.shown_locally.store(on, Ordering::Relaxed);
    }

    /// What the present thread draws into the picture.
    pub(crate) fn overlay(&self) -> Option<CursorOverlay> {
        if self.shown_locally.load(Ordering::Relaxed) {
            return None;
        }
        self.tracker.lock().ok()?.overlay()
    }
}

/// A pointer image as a window system takes it: straight RGBA, scaled by
/// `scale` (window pixels per host screen pixel), with the hotspot scaled
/// along. (width, height, hotspot, pixels.)
pub fn system_cursor(
    image: &CursorImage,
    hotspot: (u16, u16),
    scale: f64,
) -> (u16, u16, (u16, u16), Vec<u8>) {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let size = |v: u32| ((f64::from(v) * scale).round() as u32).clamp(1, 256);
    let (w, h) = (size(image.width), size(image.height));
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        // Nearest pixel of the original.
        let sy = (u64::from(y) * u64::from(image.height) / u64::from(h)) as u32;
        for x in 0..w {
            let sx = (u64::from(x) * u64::from(image.width) / u64::from(w)) as u32;
            let i = ((sy * image.width + sx) * 4) as usize;
            let (b, g, r, a) = (
                image.pixels[i],
                image.pixels[i + 1],
                image.pixels[i + 2],
                image.pixels[i + 3],
            );
            let straight = |c: u8| {
                if a == 0 {
                    0
                } else {
                    (u32::from(c) * 255 / u32::from(a)).min(255) as u8
                }
            };
            rgba.extend_from_slice(&[straight(r), straight(g), straight(b), a]);
        }
    }
    let hot = |v: u16, max: u32| ((f64::from(v) * scale).round() as u32).min(max - 1) as u16;
    (
        w as u16,
        h as u16,
        (hot(hotspot.0, w), hot(hotspot.1, h)),
        rgba,
    )
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
            hotspot: None,
        }
    }

    #[test]
    fn the_pointer_is_shown_locally_once_its_hotspot_is_known() {
        let share = PointerShare::default();
        let img = image(8, 8, 0);
        if let Ok(mut t) = share.tracker.lock() {
            feed(&mut t, 2, 8, 8, &img, None);
            t.on_position(at(10, 20, 2, true));
        }
        assert_eq!(share.local(), None, "no hotspot yet");
        assert!(share.overlay().is_some());
        if let Ok(mut t) = share.tracker.lock() {
            t.on_position(Cursor {
                hotspot: Some((1, 2)),
                ..at(10, 20, 2, true)
            });
        }
        let local = share.local().unwrap();
        assert_eq!(
            (local.serial, local.hotspot, local.screen),
            (2, (1, 2), (2560, 1440))
        );
        // While the window shows it, the picture does not.
        share.set_shown_locally(true);
        assert_eq!(share.overlay(), None);
        share.set_shown_locally(false);
        assert!(share.overlay().is_some());
        // Hidden on the host: not shown here either.
        if let Ok(mut t) = share.tracker.lock() {
            t.on_position(Cursor {
                hotspot: Some((1, 2)),
                ..at(10, 20, 2, false)
            });
        }
        assert_eq!(share.local(), None);
    }

    #[test]
    fn a_system_cursor_is_straight_rgba_scaled_with_its_hotspot() {
        // 2×1: half-transparent red (premultiplied), opaque blue.
        let img = CursorImage {
            width: 2,
            height: 1,
            pixels: vec![0, 0, 128, 128, 255, 0, 0, 255],
        };
        let (w, h, hot, rgba) = system_cursor(&img, (1, 0), 1.0);
        assert_eq!((w, h, hot), (2, 1, (1, 0)));
        assert_eq!(rgba, vec![255, 0, 0, 128, 0, 0, 255, 255]);
        // Twice the size: each pixel twice, the hotspot along.
        let (w, h, hot, rgba) = system_cursor(&img, (1, 0), 2.0);
        assert_eq!((w, h, hot), (4, 2, (2, 0)));
        assert_eq!(&rgba[..8], &[255, 0, 0, 128, 255, 0, 0, 128]);
        // Nonsense scale: as it is.
        assert_eq!(system_cursor(&img, (1, 0), f64::NAN).0, 2);
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
    fn a_missing_image_is_asked_for() {
        let mut t = CursorTracker::default();
        assert!(!t.needs_shape(), "nothing announced yet");
        t.on_position(at(0, 0, 3, true));
        assert!(t.needs_shape());
        feed(&mut t, 3, 8, 8, &image(8, 8, 0), Some(0));
        assert!(t.needs_shape(), "incomplete");
        feed(&mut t, 3, 8, 8, &image(8, 8, 0), None);
        assert!(!t.needs_shape());
        t.on_position(at(0, 0, 4, false));
        assert!(t.needs_shape(), "a new shape, even while hidden");
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
