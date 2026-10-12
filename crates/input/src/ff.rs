//! Force feedback on the host's virtual pads (D9): a game uploads rumble
//! effects to the pad and plays them; the pad's device hands both to us
//! (`include/uapi/linux/uinput.h`: an `EV_UINPUT` event asks us to take an
//! upload or erase through `UI_BEGIN_FF_*`/`UI_END_FF_*`, an `EV_FF`
//! event plays or stops an effect). What is played goes to the client as
//! a [`Rumble`].
//!
//! One thread per pad waits in `poll()` on (a duplicate of) the pad's
//! descriptor, so rumble leaves within a moment of the game asking.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crate::{Rumble, RumbleOut};

// linux/input-event-codes.h, linux/input.h, linux/uinput.h
pub const EV_FF: u16 = 0x15;
pub const FF_RUMBLE: u16 = 0x50;
const EV_UINPUT: u16 = 0x0101;
const UI_FF_UPLOAD: u16 = 1;
const UI_FF_ERASE: u16 = 2;

/// Effects a pad keeps (`uinput_setup.ff_effects_max`).
pub const MAX_EFFECTS: u32 = 16;

/// `struct ff_effect`: 48 bytes on 64-bit Linux (a 14-byte head, then the
/// union at 8-byte alignment, as large as `ff_periodic_effect` with its
/// pointer).
pub const FF_EFFECT_LEN: usize = 48;

const fn ioc(dir: u64, nr: u64, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u64) << 16) | ((b'U' as u64) << 8) | nr) as libc::c_ulong
}

/// `_IOW('U', 107, int)`.
pub const UI_SET_FFBIT: libc::c_ulong = ioc(1, 107, 4);
const UI_BEGIN_FF_UPLOAD: libc::c_ulong = ioc(3, 200, std::mem::size_of::<FfUpload>());
const UI_END_FF_UPLOAD: libc::c_ulong = ioc(1, 201, std::mem::size_of::<FfUpload>());
const UI_BEGIN_FF_ERASE: libc::c_ulong = ioc(3, 202, std::mem::size_of::<FfErase>());
const UI_END_FF_ERASE: libc::c_ulong = ioc(1, 203, std::mem::size_of::<FfErase>());

/// `struct uinput_ff_upload`.
#[repr(C, align(8))]
struct FfUpload {
    request_id: u32,
    retval: i32,
    effect: [u8; FF_EFFECT_LEN],
    old: [u8; FF_EFFECT_LEN],
}

/// `struct uinput_ff_erase`.
#[repr(C)]
struct FfErase {
    request_id: u32,
    retval: i32,
    effect_id: u32,
}

/// A rumble effect: strong and weak magnitude, and how long it plays (ms;
/// 0 until stopped).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effect {
    pub strong: u16,
    pub weak: u16,
    pub length_ms: u16,
}

/// The effect id and the rumble in a `struct ff_effect`; `None` for other
/// effect types.
pub fn rumble_of(effect: &[u8; FF_EFFECT_LEN]) -> Option<(i16, Effect)> {
    let u16_at = |i: usize| u16::from_ne_bytes([effect[i], effect[i + 1]]);
    // type, id, direction, trigger (button, interval), replay (length,
    // delay), then the union: rumble's strong and weak magnitude.
    (u16_at(0) == FF_RUMBLE).then(|| {
        (
            u16_at(2) as i16,
            Effect {
                strong: u16_at(16),
                weak: u16_at(18),
                length_ms: u16_at(10),
            },
        )
    })
}

/// A pad's uploaded effects, by id.
#[derive(Debug, Default)]
pub struct Effects {
    slots: [Option<Effect>; MAX_EFFECTS as usize],
}

impl Effects {
    pub fn upload(&mut self, id: i16, effect: Effect) {
        if let Some(slot) = usize::try_from(id).ok().and_then(|i| self.slots.get_mut(i)) {
            *slot = Some(effect);
        }
    }

    pub fn erase(&mut self, id: u32) {
        if let Some(slot) = self.slots.get_mut(id as usize) {
            *slot = None;
        }
    }

    /// What `EV_FF` (`id`, `value`: play count, 0 stops) means for pad
    /// `pad`: an effect played, or the motors stopped.
    pub fn play(&self, pad: u8, id: u16, value: i32) -> Option<Rumble> {
        let e = (*self.slots.get(usize::from(id))?)?;
        Some(if value > 0 {
            Rumble {
                pad,
                low: e.strong,
                high: e.weak,
                // Endless effects play until stopped.
                ms: if e.length_ms == 0 {
                    u32::from(u16::MAX)
                } else {
                    u32::from(e.length_ms)
                },
            }
        } else {
            Rumble {
                pad,
                ..Rumble::default()
            }
        })
    }
}

/// Takes force feedback requests from a pad's uinput device until
/// dropped.
pub struct Feedback {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Feedback {
    /// Reads `device` (a duplicate of pad `pad`'s uinput descriptor) and
    /// hands what games play to `out`.
    pub fn start(mut device: File, pad: u8, out: RumbleOut) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name(format!("rumble {}", pad + 1))
            .spawn(move || {
                let mut effects = Effects::default();
                let mut buf = [0u8; 24 * 16];
                while !s.load(Ordering::Relaxed) {
                    let mut fd = libc::pollfd {
                        fd: device.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: one valid pollfd.
                    if unsafe { libc::poll(&mut fd, 1, 100) } <= 0 {
                        continue;
                    }
                    let Ok(n) = device.read(&mut buf) else {
                        continue;
                    };
                    for e in buf[..n].as_chunks::<24>().0 {
                        let kind = u16::from_ne_bytes([e[16], e[17]]);
                        let code = u16::from_ne_bytes([e[18], e[19]]);
                        let value = i32::from_ne_bytes([e[20], e[21], e[22], e[23]]);
                        match (kind, code) {
                            (EV_UINPUT, UI_FF_UPLOAD) => {
                                upload(&device, value as u32, &mut effects)
                            }
                            (EV_UINPUT, UI_FF_ERASE) => erase(&device, value as u32, &mut effects),
                            (EV_FF, id) => {
                                if let Some(r) = effects.play(pad, id, value) {
                                    out(r);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Feedback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Takes upload `request` (the game waits in its ioctl until we end it).
fn upload(device: &File, request: u32, effects: &mut Effects) {
    let mut up = FfUpload {
        request_id: request,
        retval: 0,
        effect: [0; FF_EFFECT_LEN],
        old: [0; FF_EFFECT_LEN],
    };
    // SAFETY: the uinput ioctls with the struct they take.
    unsafe {
        if libc::ioctl(device.as_raw_fd(), UI_BEGIN_FF_UPLOAD, &mut up) < 0 {
            return;
        }
        if let Some((id, e)) = rumble_of(&up.effect) {
            effects.upload(id, e);
        }
        up.retval = 0;
        libc::ioctl(device.as_raw_fd(), UI_END_FF_UPLOAD, &up);
    }
}

fn erase(device: &File, request: u32, effects: &mut Effects) {
    let mut er = FfErase {
        request_id: request,
        retval: 0,
        effect_id: 0,
    };
    // SAFETY: as in upload().
    unsafe {
        if libc::ioctl(device.as_raw_fd(), UI_BEGIN_FF_ERASE, &mut er) < 0 {
            return;
        }
        effects.erase(er.effect_id);
        er.retval = 0;
        libc::ioctl(device.as_raw_fd(), UI_END_FF_ERASE, &er);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ioctls_match_the_kernel_headers() {
        // sizeof(struct uinput_ff_upload) = 104, uinput_ff_erase = 12.
        assert_eq!(std::mem::size_of::<FfUpload>(), 104);
        assert_eq!(std::mem::size_of::<FfErase>(), 12);
        assert_eq!(UI_SET_FFBIT, 0x4004_556b);
        assert_eq!(UI_BEGIN_FF_UPLOAD, 0xc068_55c8);
        assert_eq!(UI_END_FF_UPLOAD, 0x4068_55c9);
        assert_eq!(UI_BEGIN_FF_ERASE, 0xc00c_55ca);
        assert_eq!(UI_END_FF_ERASE, 0x400c_55cb);
    }

    fn rumble_effect(id: i16, strong: u16, weak: u16, length: u16) -> [u8; FF_EFFECT_LEN] {
        let mut e = [0u8; FF_EFFECT_LEN];
        e[0..2].copy_from_slice(&FF_RUMBLE.to_ne_bytes());
        e[2..4].copy_from_slice(&id.to_ne_bytes());
        e[10..12].copy_from_slice(&length.to_ne_bytes());
        e[16..18].copy_from_slice(&strong.to_ne_bytes());
        e[18..20].copy_from_slice(&weak.to_ne_bytes());
        e
    }

    #[test]
    fn a_rumble_effect_is_read_and_played() {
        let (id, e) = rumble_of(&rumble_effect(3, 0xffff, 0x4000, 250)).unwrap();
        assert_eq!(id, 3);
        let mut effects = Effects::default();
        effects.upload(id, e);
        let want = Rumble {
            pad: 1,
            low: 0xffff,
            high: 0x4000,
            ms: 250,
        };
        assert_eq!(effects.play(1, 3, 1), Some(want));
        assert_eq!(
            effects.play(1, 3, 0),
            Some(Rumble {
                pad: 1,
                ..Rumble::default()
            }),
            "stopped"
        );
        assert_eq!(effects.play(1, 4, 1), None, "never uploaded");
        effects.erase(3);
        assert_eq!(effects.play(1, 3, 1), None, "erased");
        // An endless effect plays until stopped.
        effects.upload(5, rumble_of(&rumble_effect(5, 1, 2, 0)).unwrap().1);
        assert_eq!(effects.play(0, 5, 1).unwrap().ms, 65_535);
        // Other effect types are not rumble; ids beyond the slots are not
        // kept.
        let mut periodic = rumble_effect(0, 1, 1, 1);
        periodic[0] = 0x51;
        assert_eq!(rumble_of(&periodic), None);
        effects.upload(99, e);
        assert_eq!(effects.play(0, 99, 1), None);
    }
}
