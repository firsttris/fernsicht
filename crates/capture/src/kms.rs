//! Capture of the scanout plane over KMS/DRM.
//!
//! Each frame: wait for the vblank of the monitor's CRTC, read which
//! framebuffer its primary plane shows, and export that framebuffer as a
//! DMA-BUF. Nothing is copied; the encoder imports the buffer directly.
//! This is what Sunshine's KMS backend does. It works without a desktop
//! session (login screen, gamescope) and needs no confirmation dialog.
//!
//! The pointer is on its own plane (the cursor plane) and not in the
//! picture. It is reported with every frame instead ([`Frame::cursor`]):
//! its position each frame, its image when it changes. Overlay planes are
//! not captured.
//!
//! The kernel only hands out buffer handles of another client's
//! framebuffer to processes with `CAP_SYS_ADMIN`. Without it, GetFB2
//! returns no handles and [`KmsCapture::open`] fails with
//! [`NEEDS_CAP_SYS_ADMIN`] in its message; see docs/kms-capture.md.

use std::fs::OpenOptions;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drm::control::{self, Device as _, framebuffer, plane};
use drm::{ClientCapability, Device as _, VblankWaitFlags, VblankWaitTarget};
use fernsicht_core::{clock, now_us};

use crate::dmabuf::{DmaBuf, DmaBufPlane, formats};
use crate::{CaptureError, CursorImage, CursorState, Frame, FrameSource, PixelFormat};

/// Part of the error message when the process lacks the capability.
pub const NEEDS_CAP_SYS_ADMIN: &str = "KMS capture needs CAP_SYS_ADMIN";

/// What to capture.
#[derive(Clone, Debug, Default)]
pub struct KmsConfig {
    /// DRM card node, e.g. `/dev/dri/card1`. `None`: the first card with
    /// an active display.
    pub card: Option<PathBuf>,
    /// Connector name as the kernel calls it, e.g. `DP-1` or `HDMI-A-1`.
    /// `None`: the first active one.
    pub connector: Option<String>,
    /// Frames per second to deliver. Faster monitors are sampled at the
    /// vblank closest to the frame grid.
    pub fps: u32,
}

struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl control::Device for Card {}

fn backend(what: &str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("{what}: {e}"))
}

/// A plane as far as choosing one is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaneCandidate {
    pub id: u32,
    pub primary: bool,
    pub crtc: Option<u32>,
    pub fb: Option<u32>,
}

/// A CRTC (one display pipeline) with the connectors it drives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrtcCandidate {
    pub id: u32,
    /// Position in the card's CRTC list: the vblank "pipe".
    pub index: u32,
    pub active: bool,
    pub connectors: Vec<String>,
}

/// The chosen plane, its CRTC and the CRTC's pipe index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub plane: u32,
    pub crtc: u32,
    pub pipe: u32,
}

/// Picks the primary plane of the wanted (or the first active) display.
pub fn choose(
    planes: &[PlaneCandidate],
    crtcs: &[CrtcCandidate],
    connector: Option<&str>,
) -> Result<Selection, String> {
    let active: Vec<&CrtcCandidate> = crtcs
        .iter()
        .filter(|c| c.active && !c.connectors.is_empty())
        .collect();
    let crtc = match connector {
        Some(name) => active
            .iter()
            .find(|c| c.connectors.iter().any(|n| n == name))
            .ok_or_else(|| {
                let names: Vec<&str> = active
                    .iter()
                    .flat_map(|c| c.connectors.iter().map(String::as_str))
                    .collect();
                format!("connector {name} is not active (active: {names:?})")
            })?,
        None => active.first().ok_or("no active display")?,
    };
    let plane = planes
        .iter()
        .find(|p| p.primary && p.crtc == Some(crtc.id) && p.fb.is_some())
        .ok_or_else(|| {
            format!(
                "CRTC {} has no primary plane showing a framebuffer",
                crtc.id
            )
        })?;
    Ok(Selection {
        plane: plane.id,
        crtc: crtc.id,
        pipe: crtc.index,
    })
}

/// Whether a vblank at `t` is the one to capture for a frame due at `due`,
/// given the monitor's refresh period: the first vblank no more than half a
/// refresh period before the due time.
pub fn vblank_is_due(t: Instant, due: Instant, refresh: Duration) -> bool {
    t + refresh / 2 >= due
}

pub struct KmsCapture {
    card: Card,
    sel: Selection,
    /// The cursor plane of our CRTC, if the driver has one.
    cursor: Option<CursorPlane>,
    width: u32,
    height: u32,
    refresh: Duration,
    interval: Duration,
    next_due: Option<Instant>,
    vblank_works: bool,
    seq: u64,
}

impl KmsCapture {
    pub fn open(cfg: &KmsConfig) -> Result<Self, CaptureError> {
        let cards = match &cfg.card {
            Some(card) => vec![card.clone()],
            None => list_cards(Path::new("/dev/dri")),
        };
        let mut errors = Vec::new();
        for path in cards {
            match Self::open_card(&path, cfg) {
                Ok(cap) => return Ok(cap),
                // A missing capability is the same on every card.
                Err(e) if e.to_string().contains(NEEDS_CAP_SYS_ADMIN) => return Err(e),
                Err(e) => errors.push(format!("{}: {e}", path.display())),
            }
        }
        Err(CaptureError::Backend(format!(
            "no display to capture ({})",
            if errors.is_empty() {
                "no /dev/dri/card* found".into()
            } else {
                errors.join("; ")
            }
        )))
    }

    fn open_card(path: &Path, cfg: &KmsConfig) -> Result<Self, CaptureError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| backend("open", e))?;
        let card = Card(file);
        card.set_client_capability(ClientCapability::UniversalPlanes, true)
            .map_err(|e| backend("enable universal planes", e))?;
        let res = card
            .resource_handles()
            .map_err(|e| backend("read KMS resources", e))?;

        let mut crtcs = Vec::new();
        let mut refresh = std::collections::HashMap::new();
        for (index, &h) in res.crtcs().iter().enumerate() {
            let info = card.get_crtc(h).map_err(|e| backend("read CRTC", e))?;
            if let Some(mode) = info.mode() {
                refresh.insert(u32::from(h), mode.vrefresh());
            }
            crtcs.push(CrtcCandidate {
                id: h.into(),
                index: index as u32,
                active: info.mode().is_some(),
                connectors: Vec::new(),
            });
        }
        for &h in res.connectors() {
            let Ok(info) = card.get_connector(h, false) else {
                continue;
            };
            if info.state() != control::connector::State::Connected {
                continue;
            }
            let crtc = info
                .current_encoder()
                .and_then(|e| card.get_encoder(e).ok())
                .and_then(|e| e.crtc());
            if let Some(crtc) = crtc {
                let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
                if let Some(c) = crtcs.iter_mut().find(|c| c.id == u32::from(crtc)) {
                    c.connectors.push(name);
                }
            }
        }
        let mut planes = Vec::new();
        for h in card
            .plane_handles()
            .map_err(|e| backend("list planes", e))?
        {
            let info = card.get_plane(h).map_err(|e| backend("read plane", e))?;
            planes.push(PlaneCandidate {
                id: h.into(),
                primary: is_primary(&card, h),
                crtc: info.crtc().map(Into::into),
                fb: info.framebuffer().map(Into::into),
            });
        }
        let sel =
            choose(&planes, &crtcs, cfg.connector.as_deref()).map_err(CaptureError::Backend)?;
        let hz = refresh.get(&sel.crtc).copied().unwrap_or(60).max(1);

        let cursor = find_cursor_plane(&card, &res, sel.crtc);
        if cursor.is_none() {
            log::info!("no cursor plane for the captured display; the pointer is not reported");
        }
        let mut cap = Self {
            cursor,
            card,
            sel,
            width: 0,
            height: 0,
            refresh: Duration::from_micros(1_000_000 / u64::from(hz)),
            interval: Duration::from_micros(clock::frame_interval_us(cfg.fps)),
            next_due: None,
            vblank_works: true,
            seq: 0,
        };
        // Export once: fails early without the capability and tells us the
        // framebuffer size.
        let first = cap.export()?;
        cap.width = first.width;
        cap.height = first.height;
        Ok(cap)
    }

    /// The plane, CRTC and pipe being captured.
    pub fn selection(&self) -> Selection {
        self.sel
    }

    /// Exports the framebuffer the plane shows right now.
    fn export(&self) -> Result<DmaBuf, CaptureError> {
        let plane = self
            .card
            .get_plane(plane::Handle::from(nonzero(self.sel.plane)?))
            .map_err(|e| backend("read plane", e))?;
        let fb: framebuffer::Handle = plane
            .framebuffer()
            .ok_or_else(|| CaptureError::Backend("display is off (no framebuffer)".into()))?;
        let info = self
            .card
            .get_planar_framebuffer(fb)
            .map_err(|e| backend("read framebuffer (GetFB2)", e))?;

        let handles = info.buffers();
        // GetFB2 gives us new GEM handles; close them whatever happens.
        let mut unique: Vec<drm::buffer::Handle> = Vec::new();
        for h in handles.iter().flatten() {
            if !unique.contains(h) {
                unique.push(*h);
            }
        }
        let result = (|| {
            if handles[0].is_none() {
                return Err(CaptureError::Backend(format!(
                    "{NEEDS_CAP_SYS_ADMIN} (the kernel withheld the framebuffer's buffer handles)"
                )));
            }
            let mut objects = Vec::new();
            for &h in &unique {
                let fd = self
                    .card
                    .buffer_to_prime_fd(h, libc::O_CLOEXEC as u32)
                    .map_err(|e| backend("export framebuffer as DMA-BUF", e))?;
                objects.push(Arc::new(fd));
            }
            let mut planes = Vec::new();
            for (i, h) in handles.iter().enumerate() {
                let Some(h) = h else { break };
                planes.push(DmaBufPlane {
                    object: unique.iter().position(|u| u == h).expect("collected above"),
                    offset: info.offsets()[i],
                    pitch: info.pitches()[i],
                });
            }
            let (width, height) = info.size();
            Ok(DmaBuf {
                width,
                height,
                fourcc: info.pixel_format() as u32,
                modifier: info.modifier().map_or(formats::MOD_INVALID, u64::from),
                objects,
                planes,
            })
        })();
        for h in unique {
            let _ = self.card.close_buffer(h);
        }
        result
    }

    /// Waits for the vblank that starts the next frame and returns its
    /// time on our clock (µs).
    fn wait_for_frame(&mut self) -> u64 {
        let due = *self.next_due.get_or_insert_with(Instant::now);
        let captured = loop {
            if self.vblank_works {
                match self.card.wait_vblank(
                    VblankWaitTarget::Relative(1),
                    VblankWaitFlags::empty(),
                    self.sel.pipe,
                    0,
                ) {
                    Ok(reply) => {
                        let at = reply.time().map_or_else(Instant::now, monotonic_to_instant);
                        if vblank_is_due(at, due, self.refresh) {
                            break at;
                        }
                        continue;
                    }
                    Err(e) => {
                        log::warn!("vblank wait failed ({e}); pacing by timer");
                        self.vblank_works = false;
                    }
                }
            }
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            break Instant::now();
        };
        // Stay on the grid; after a stall, restart from now instead of
        // bursting.
        let mut next = due + self.interval;
        if next < captured {
            next = captured + self.interval;
        }
        self.next_due = Some(next);
        let age = Instant::now().saturating_duration_since(captured);
        now_us().saturating_sub(age.as_micros() as u64)
    }
}

fn nonzero(id: u32) -> Result<control::RawResourceHandle, CaptureError> {
    control::RawResourceHandle::new(id)
        .ok_or_else(|| CaptureError::Backend("invalid KMS object id 0".into()))
}

fn is_primary(card: &Card, plane: plane::Handle) -> bool {
    plane_property(card, plane, "type") == Some(control::PlaneType::Primary as u64)
}

/// The raw value of a plane property, by name.
fn plane_property(card: &Card, plane: plane::Handle, name: &str) -> Option<u64> {
    let props = card.get_properties(plane).ok()?;
    props.iter().find_map(|(id, value)| {
        card.get_property(*id)
            .is_ok_and(|p| p.name().to_bytes() == name.as_bytes())
            .then_some(*value)
    })
}

/// How often the cursor image is read again although its framebuffer did
/// not change (a compositor may redraw into the same buffer).
const CURSOR_REFRESH: Duration = Duration::from_secs(2);

/// The cursor plane and what we last read from it.
struct CursorPlane {
    plane: plane::Handle,
    /// Framebuffer the image was read from, and when.
    fb: Option<(u32, Instant)>,
    /// The visible part of the image and where it starts in the buffer.
    image: Option<(Arc<CursorImage>, u32, u32)>,
    serial: u32,
    failed: bool,
}

/// A cursor plane that can show on `crtc`.
fn find_cursor_plane(
    card: &Card,
    res: &control::ResourceHandles,
    crtc: u32,
) -> Option<CursorPlane> {
    card.plane_handles().ok()?.into_iter().find_map(|h| {
        let info = card.get_plane(h).ok()?;
        let fits = res
            .filter_crtcs(info.possible_crtcs())
            .iter()
            .any(|c| u32::from(*c) == crtc);
        let cursor = plane_property(card, h, "type") == Some(control::PlaneType::Cursor as u64);
        (fits && cursor).then_some(CursorPlane {
            plane: h,
            fb: None,
            image: None,
            serial: 0,
            failed: false,
        })
    })
}

/// A KMS signed-range property value (stored as two's complement).
fn signed(v: u64) -> i32 {
    v as i64 as i32
}

impl KmsCapture {
    /// The pointer now: position every frame, image when it changed. A
    /// failure only switches pointer reporting off; capture goes on.
    fn read_cursor(&mut self) -> Option<CursorState> {
        let crtc = self.sel.crtc;
        let cp = self.cursor.as_mut()?;
        if cp.failed {
            return None;
        }
        let info = self.card.get_plane(cp.plane).ok()?;
        let fb = info
            .framebuffer()
            .filter(|_| info.crtc().is_some_and(|c| u32::from(c) == crtc));
        let Some(fb) = fb else {
            // Hidden; say so if a pointer was shown before.
            let (image, dx, dy) = cp.image.clone()?;
            return Some(CursorState {
                visible: false,
                x: dx as i32,
                y: dy as i32,
                serial: cp.serial,
                image,
            });
        };
        let fb_id = u32::from(fb);
        let stale = cp
            .fb
            .is_none_or(|(id, at)| id != fb_id || at.elapsed() >= CURSOR_REFRESH);
        if stale {
            match read_cursor_image(&self.card, fb) {
                Ok(raw) => {
                    cp.fb = Some((fb_id, Instant::now()));
                    let trimmed = raw.trimmed();
                    let changed = match (&cp.image, &trimmed) {
                        (Some((old, ox, oy)), Some((new, nx, ny))) => {
                            **old != *new || (ox, oy) != (nx, ny)
                        }
                        (None, None) => false,
                        _ => true,
                    };
                    if changed {
                        cp.serial = cp.serial.wrapping_add(1).max(1);
                        cp.image = trimmed.map(|(img, dx, dy)| (Arc::new(img), dx, dy));
                    }
                }
                Err(e) => {
                    log::warn!("cannot read the pointer image ({e}); the pointer is not reported");
                    cp.failed = true;
                    return None;
                }
            }
        }
        let (image, dx, dy) = cp.image.clone()?;
        let x = plane_property(&self.card, cp.plane, "CRTC_X").map_or(0, signed);
        let y = plane_property(&self.card, cp.plane, "CRTC_Y").map_or(0, signed);
        Some(CursorState {
            visible: true,
            x: x + dx as i32,
            y: y + dy as i32,
            serial: cp.serial,
            image,
        })
    }
}

/// `DMA_BUF_IOCTL_SYNC` and its flags (linux/dma-buf.h).
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_END: u64 = 4;

/// Copies a cursor framebuffer (linear ARGB8888, as cursor planes use) to
/// memory through a mapping of its DMA-BUF.
fn read_cursor_image(card: &Card, fb: framebuffer::Handle) -> Result<CursorImage, String> {
    let info = card
        .get_planar_framebuffer(fb)
        .map_err(|e| format!("GetFB2: {e}"))?;
    let format = info.pixel_format() as u32;
    if format != formats::ARGB8888 {
        return Err(format!("format {}", crate::dmabuf::fourcc_name(format)));
    }
    if info
        .modifier()
        .is_some_and(|m| !matches!(u64::from(m), formats::MOD_LINEAR | formats::MOD_INVALID))
    {
        return Err("tiled cursor buffer".into());
    }
    let handle = info.buffers()[0].ok_or(NEEDS_CAP_SYS_ADMIN)?;
    let result = (|| {
        let fd = card
            .buffer_to_prime_fd(handle, libc::O_CLOEXEC as u32)
            .map_err(|e| format!("export: {e}"))?;
        let (w, h) = info.size();
        let (pitch, offset) = (info.pitches()[0] as usize, info.offsets()[0] as usize);
        let row = w as usize * 4;
        if pitch < row || w == 0 || h == 0 || w > 512 || h > 512 {
            return Err(format!("odd cursor buffer {w}×{h}, pitch {pitch}"));
        }
        let len = offset + pitch * h as usize;
        use std::os::fd::AsRawFd;
        // SAFETY: a read-only shared mapping of a DMA-BUF we own; unmapped
        // below; reads stay within `len`.
        unsafe {
            let map = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            );
            if map == libc::MAP_FAILED {
                return Err(format!("mmap: {}", std::io::Error::last_os_error()));
            }
            let sync = |flags: u64| libc::ioctl(fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC, &flags);
            sync(DMA_BUF_SYNC_READ);
            let base = (map as *const u8).add(offset);
            let mut pixels = Vec::with_capacity(row * h as usize);
            for y in 0..h as usize {
                pixels.extend_from_slice(std::slice::from_raw_parts(base.add(y * pitch), row));
            }
            sync(DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END);
            libc::munmap(map, len);
            Ok(CursorImage {
                width: w,
                height: h,
                pixels,
            })
        }
    })();
    let _ = card.close_buffer(handle);
    result
}

/// `/dev/dri/card*`, in order.
fn list_cards(dir: &Path) -> Vec<PathBuf> {
    let mut cards: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("card") && n[4..].parse::<u32>().is_ok())
        })
        .collect();
    cards.sort();
    cards
}

/// Converts a CLOCK_MONOTONIC timestamp (what vblank events carry) into an
/// `Instant`, which uses the same clock on Linux.
fn monotonic_to_instant(t: Duration) -> Instant {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    let mono_now = Duration::new(now.tv_sec as u64, now.tv_nsec as u32);
    let age = mono_now.saturating_sub(t);
    Instant::now().checked_sub(age).unwrap_or_else(Instant::now)
}

impl FrameSource for KmsCapture {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn format(&self) -> PixelFormat {
        PixelFormat::Bgrx
    }

    fn next_frame(&mut self, frame: &mut Frame) -> Result<(), CaptureError> {
        let captured_us = self.wait_for_frame();
        let image = self.export()?;
        frame.cursor = self.read_cursor();
        frame.width = image.width;
        frame.height = image.height;
        frame.format = PixelFormat::Bgrx;
        frame.data.clear();
        frame.dmabuf = Some(image);
        frame.seq = self.seq;
        self.seq += 1;
        frame.capture_us = captured_us;
        frame.ready_us = now_us();
        Ok(())
    }

    /// Frames carry DMA-BUFs only, so no pixel memory is allocated.
    fn alloc_frame(&self) -> Frame {
        let mut f = Frame::new(0, 0, PixelFormat::Bgrx);
        f.width = self.width;
        f.height = self.height;
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crtc(id: u32, index: u32, active: bool, connectors: &[&str]) -> CrtcCandidate {
        CrtcCandidate {
            id,
            index,
            active,
            connectors: connectors.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn plane(id: u32, primary: bool, crtc: Option<u32>, fb: Option<u32>) -> PlaneCandidate {
        PlaneCandidate {
            id,
            primary,
            crtc,
            fb,
        }
    }

    fn setup() -> (Vec<PlaneCandidate>, Vec<CrtcCandidate>) {
        (
            vec![
                plane(31, false, Some(80), Some(200)), // overlay on DP-1
                plane(32, true, None, None),           // unused primary
                plane(33, true, Some(80), Some(201)),  // primary of DP-1
                plane(34, true, Some(81), Some(202)),  // primary of HDMI-A-1
                plane(35, false, Some(81), None),      // cursor-ish
            ],
            vec![
                crtc(79, 0, false, &[]),
                crtc(80, 1, true, &["DP-1"]),
                crtc(81, 2, true, &["HDMI-A-1"]),
            ],
        )
    }

    #[test]
    fn first_active_display_by_default() {
        let (p, c) = setup();
        assert_eq!(
            choose(&p, &c, None),
            Ok(Selection {
                plane: 33,
                crtc: 80,
                pipe: 1
            })
        );
    }

    #[test]
    fn display_by_connector_name() {
        let (p, c) = setup();
        assert_eq!(
            choose(&p, &c, Some("HDMI-A-1")),
            Ok(Selection {
                plane: 34,
                crtc: 81,
                pipe: 2
            })
        );
        let err = choose(&p, &c, Some("DP-2")).unwrap_err();
        assert!(err.contains("DP-2") && err.contains("HDMI-A-1"), "{err}");
    }

    #[test]
    fn overlay_planes_and_dark_displays_are_not_chosen() {
        let (mut p, c) = setup();
        p.retain(|p| p.id != 33);
        // DP-1 is lit but only an overlay shows something: no primary.
        assert!(choose(&p, &c, Some("DP-1")).is_err());
        assert!(choose(&p, &[crtc(80, 0, false, &["DP-1"])], None).is_err());
        assert!(choose(&[], &[], None).is_err());
    }

    #[test]
    fn active_crtc_without_connector_is_skipped() {
        let (p, mut c) = setup();
        c[1].connectors.clear();
        assert_eq!(choose(&p, &c, None).unwrap().crtc, 81);
    }

    #[test]
    fn signed_plane_positions() {
        assert_eq!(signed(5), 5);
        assert_eq!(signed((-12i64) as u64), -12);
        assert_eq!(signed(u64::MAX), -1);
    }

    #[test]
    fn vblank_sampling_on_fast_monitors() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // 144 Hz monitor (6.9 ms), 60 fps stream: a vblank 2 ms before the
        // due time is taken, one 5 ms before is skipped.
        let refresh = Duration::from_micros(6944);
        let due = t0 + ms(16);
        assert!(vblank_is_due(t0 + ms(14), due, refresh));
        assert!(!vblank_is_due(t0 + ms(11), due, refresh));
        assert!(vblank_is_due(t0 + ms(20), due, refresh));
        // 60 Hz monitor, 60 fps: every vblank is due even with jitter.
        let refresh = Duration::from_micros(16_667);
        assert!(vblank_is_due(t0 + ms(15), due, refresh));
    }

    #[test]
    fn card_nodes_are_listed_in_order() {
        let dir = std::env::temp_dir().join(format!("fernsicht-dri-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["card1", "renderD128", "card0", "card-x", "by-path"] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        let cards = list_cards(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(cards, vec![dir.join("card0"), dir.join("card1")]);
        assert!(list_cards(Path::new("/does/not/exist")).is_empty());
    }

    #[test]
    fn monotonic_conversion_is_close_to_now() {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid out-pointer.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        let mono = Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);
        let at = monotonic_to_instant(mono - Duration::from_millis(5));
        let age = Instant::now() - at;
        assert!(
            age >= Duration::from_millis(5) && age < Duration::from_millis(50),
            "{age:?}"
        );
    }

    #[test]
    fn missing_card_is_a_clear_error() {
        let cfg = KmsConfig {
            card: Some("/dev/dri/card-does-not-exist".into()),
            connector: None,
            fps: 60,
        };
        let err = KmsCapture::open(&cfg).err().unwrap().to_string();
        assert!(err.contains("card-does-not-exist"), "{err}");
    }
}
