//! The mouse pointer, captured separately from the screen image.
//!
//! On real hardware the pointer lives on its own plane (the KMS cursor
//! plane), so it is not in the captured framebuffer. Capture backends
//! report it next to the frame; the client draws it on top.

use std::sync::Arc;

/// A pointer image: 4 bytes per pixel in DRM `ARGB8888` memory order
/// (B, G, R, A), premultiplied alpha, rows without padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl CursorImage {
    /// The smallest part that has visible pixels, and where it starts in
    /// the original. Cursor planes are 64×64 or 256×256 with the arrow in a
    /// corner; trimming keeps what goes over the network small. `None` if
    /// nothing is visible.
    pub fn trimmed(&self) -> Option<(CursorImage, u32, u32)> {
        let (w, h) = (self.width as usize, self.height as usize);
        if self.pixels.len() < w * h * 4 {
            return None;
        }
        let visible = |x: usize, y: usize| self.pixels[(y * w + x) * 4 + 3] != 0;
        let rows: Vec<usize> = (0..h).filter(|&y| (0..w).any(|x| visible(x, y))).collect();
        let (&top, &bottom) = (rows.first()?, rows.last()?);
        let left = (0..w).find(|&x| (top..=bottom).any(|y| visible(x, y)))?;
        let right = (0..w)
            .rev()
            .find(|&x| (top..=bottom).any(|y| visible(x, y)))?;
        let (tw, th) = (right - left + 1, bottom - top + 1);
        let mut pixels = Vec::with_capacity(tw * th * 4);
        for y in top..=bottom {
            pixels.extend_from_slice(&self.pixels[(y * w + left) * 4..(y * w + right + 1) * 4]);
        }
        Some((
            CursorImage {
                width: tw as u32,
                height: th as u32,
                pixels,
            },
            left as u32,
            top as u32,
        ))
    }

    /// A classic arrow, 12×19, white with a black outline (the test
    /// pattern's pointer).
    pub fn arrow() -> CursorImage {
        const ROWS: [&str; 19] = [
            "B           ",
            "BB          ",
            "BWB         ",
            "BWWB        ",
            "BWWWB       ",
            "BWWWWB      ",
            "BWWWWWB     ",
            "BWWWWWWB    ",
            "BWWWWWWWB   ",
            "BWWWWWWWWB  ",
            "BWWWWWWWWWB ",
            "BWWWWWWBBBBB",
            "BWWWBWWB    ",
            "BWWB BWWB   ",
            "BWB  BWWB   ",
            "BB    BWWB  ",
            "B     BWWB  ",
            "       BWWB ",
            "        BB  ",
        ];
        let mut pixels = Vec::with_capacity(12 * 19 * 4);
        for row in ROWS {
            for c in row.bytes() {
                pixels.extend_from_slice(match c {
                    b'W' => &[255, 255, 255, 255],
                    b'B' => &[0, 0, 0, 255],
                    _ => &[0, 0, 0, 0],
                });
            }
        }
        CursorImage {
            width: 12,
            height: 19,
            pixels,
        }
    }
}

/// The pointer at the time of a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorState {
    pub visible: bool,
    /// Top-left of `image` on the screen, in screen pixels; may be negative.
    pub x: i32,
    pub y: i32,
    /// Changes whenever the image changes; never 0.
    pub serial: u32,
    pub image: Arc<CursorImage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank(w: u32, h: u32) -> CursorImage {
        CursorImage {
            width: w,
            height: h,
            pixels: vec![0; (w * h * 4) as usize],
        }
    }

    #[test]
    fn trimming_finds_the_visible_part() {
        let mut img = blank(64, 64);
        let arrow = CursorImage::arrow();
        // The arrow at (5, 7) inside a 64×64 plane buffer.
        for y in 0..arrow.height as usize {
            for x in 0..arrow.width as usize {
                let src = &arrow.pixels[(y * 12 + x) * 4..][..4];
                img.pixels[((y + 7) * 64 + x + 5) * 4..][..4].copy_from_slice(src);
            }
        }
        let (t, dx, dy) = img.trimmed().unwrap();
        assert_eq!((dx, dy), (5, 7));
        assert_eq!(t, arrow);
    }

    #[test]
    fn invisible_or_broken_images_trim_to_nothing() {
        assert_eq!(blank(64, 64).trimmed(), None);
        let short = CursorImage {
            width: 64,
            height: 64,
            pixels: vec![255; 10],
        };
        assert_eq!(short.trimmed(), None);
        let mut one = blank(4, 4);
        one.pixels[(2 * 4 + 3) * 4 + 3] = 1;
        let (t, dx, dy) = one.trimmed().unwrap();
        assert_eq!((t.width, t.height, dx, dy), (1, 1, 3, 2));
    }

    #[test]
    fn arrow_is_opaque_where_drawn() {
        let a = CursorImage::arrow();
        assert_eq!(a.pixels.len(), 12 * 19 * 4);
        assert_eq!(&a.pixels[..4], &[0, 0, 0, 255], "tip is outline");
        assert_eq!(a.trimmed().unwrap().0, a, "no transparent border");
    }
}
