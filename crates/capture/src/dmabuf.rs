//! Frames that stay on the GPU: a DMA-BUF description as KMS and PipeWire
//! hand it out, imported by the encoder without a copy.

use std::os::fd::OwnedFd;
use std::sync::Arc;

use crate::CaptureError;

/// Builds a DRM fourcc code from its four characters.
pub const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

/// DRM pixel formats the capture backends produce. The names follow
/// `drm_fourcc.h`: components are listed from the most significant bit of a
/// little-endian 32-bit word, so `XRGB8888` is B, G, R, X in memory.
pub mod formats {
    use super::fourcc;

    pub const XRGB8888: u32 = fourcc(b"XR24");
    pub const ARGB8888: u32 = fourcc(b"AR24");
    pub const XBGR8888: u32 = fourcc(b"XB24");
    pub const ABGR8888: u32 = fourcc(b"AB24");
    pub const XRGB2101010: u32 = fourcc(b"XR30");

    /// `DRM_FORMAT_MOD_LINEAR`.
    pub const MOD_LINEAR: u64 = 0;
    /// `DRM_FORMAT_MOD_INVALID`: the layout is implied by the driver.
    pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
}

/// Renders a fourcc for log messages, e.g. `XR24`.
pub fn fourcc_name(code: u32) -> String {
    code.to_le_bytes()
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}

/// One plane of a DMA-BUF image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaBufPlane {
    /// Index into [`DmaBuf::objects`].
    pub object: usize,
    pub offset: u32,
    pub pitch: u32,
}

/// An image in GPU memory, shared as DMA-BUF file descriptors.
///
/// Single-plane RGB is the common case. Compressed layouts (AMD DCC, Intel
/// CCS) add metadata planes, usually in the same buffer object at another
/// offset, which is why planes refer to objects by index.
#[derive(Clone, Debug)]
pub struct DmaBuf {
    pub width: u32,
    pub height: u32,
    /// DRM fourcc, see [`formats`].
    pub fourcc: u32,
    /// DRM format modifier (tiling, compression), see [`formats::MOD_LINEAR`].
    pub modifier: u64,
    /// Buffer objects; shared so a frame can be cloned cheaply. The image
    /// stays alive as long as any descriptor is open.
    pub objects: Vec<Arc<OwnedFd>>,
    pub planes: Vec<DmaBufPlane>,
}

/// Most planes a DRM framebuffer can have.
pub const MAX_PLANES: usize = 4;

impl DmaBuf {
    /// Checks the description before it reaches a driver.
    pub fn validate(&self) -> Result<(), CaptureError> {
        let bad = |why: &str| Err(CaptureError::Backend(format!("invalid DMA-BUF: {why}")));
        if self.width == 0 || self.height == 0 {
            return bad("empty image");
        }
        if self.objects.is_empty() || self.objects.len() > MAX_PLANES {
            return bad("needs 1 to 4 buffer objects");
        }
        if self.planes.is_empty() || self.planes.len() > MAX_PLANES {
            return bad("needs 1 to 4 planes");
        }
        if self.planes.iter().any(|p| p.object >= self.objects.len()) {
            return bad("plane refers to a missing buffer object");
        }
        if self.planes[0].pitch == 0 {
            return bad("first plane has no pitch");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fd() -> Arc<OwnedFd> {
        Arc::new(std::fs::File::open("/dev/null").unwrap().into())
    }

    fn image() -> DmaBuf {
        DmaBuf {
            width: 1920,
            height: 1080,
            fourcc: formats::XRGB8888,
            modifier: formats::MOD_LINEAR,
            objects: vec![fd()],
            planes: vec![DmaBufPlane {
                object: 0,
                offset: 0,
                pitch: 7680,
            }],
        }
    }

    #[test]
    fn fourcc_matches_drm_fourcc_h() {
        // Values from drm_fourcc.h.
        assert_eq!(formats::XRGB8888, 0x3432_5258);
        assert_eq!(formats::ARGB8888, 0x3432_5241);
        assert_eq!(formats::XBGR8888, 0x3432_4258);
        assert_eq!(formats::XRGB2101010, 0x3033_5258);
        assert_eq!(fourcc_name(formats::XRGB8888), "XR24");
        assert_eq!(fourcc_name(0x0000_4142), "BA??");
    }

    #[test]
    fn valid_image_passes() {
        image().validate().unwrap();
        let mut dcc = image();
        dcc.planes.push(DmaBufPlane {
            object: 0,
            offset: 8_294_400,
            pitch: 256,
        });
        dcc.validate().unwrap();
    }

    #[test]
    fn broken_descriptions_are_rejected() {
        type Breaker = fn(&mut DmaBuf);
        let cases: [(&str, Breaker); 6] = [
            ("empty", |d| d.width = 0),
            ("no objects", |d| d.objects.clear()),
            ("no planes", |d| d.planes.clear()),
            ("dangling plane", |d| d.planes[0].object = 1),
            ("no pitch", |d| d.planes[0].pitch = 0),
            ("too many planes", |d| {
                d.planes = vec![d.planes[0]; 5];
            }),
        ];
        for (name, break_it) in cases {
            let mut d = image();
            break_it(&mut d);
            assert!(d.validate().is_err(), "{name} must be rejected");
        }
    }

    #[test]
    fn clones_share_the_descriptor() {
        let a = image();
        let b = a.clone();
        assert!(Arc::ptr_eq(&a.objects[0], &b.objects[0]));
    }
}
