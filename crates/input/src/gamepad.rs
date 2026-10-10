//! Gamepads on the client, read straight from the kernel (evdev, no
//! extra library), as protocol events for up to [`MAX_PADS`] pads.
//!
//! Every pad with face buttons and a stick counts. Sticks, triggers and
//! the d-pad are scaled to the protocol's ranges whatever the device
//! reports; d-pads as buttons and digital triggers become axes. Buttons
//! travel by position (`BTN_SOUTH` = bottom); pads with the `xpad`
//! layout, where X and Y are swapped, are turned around.
//!
//! Our own virtual pads (the host's, if client and host share a machine)
//! are skipped, or they would feed themselves. Pads come and go: the
//! directory is looked at again every two seconds, and a pad that is
//! unplugged leaves its buttons released and sticks centered.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fernsicht_proto::{BTN_PAD_FIRST, BTN_PAD_LAST, InputEvent, MAX_PADS, PadAxis};

const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const BTN_SOUTH: u16 = 0x130;
const BTN_NORTH: u16 = 0x133;
const BTN_WEST: u16 = 0x134;
const BTN_TL2: u16 = 0x138;
const BTN_TR2: u16 = 0x139;
const BTN_DPAD_UP: u16 = 0x220;
const BTN_DPAD_DOWN: u16 = 0x221;
const BTN_DPAD_LEFT: u16 = 0x222;
const BTN_DPAD_RIGHT: u16 = 0x223;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_GAS: u16 = 0x09;
const ABS_BRAKE: u16 = 0x0a;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

/// How often the device directory is looked at for new pads.
const RESCAN: Duration = Duration::from_secs(2);

/// One device axis and how it maps to the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisMap {
    pub axis: PadAxis,
    pub min: i32,
    pub max: i32,
}

/// What a device reports and how to read it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mapping {
    /// Device axis code → protocol axis.
    pub axes: BTreeMap<u16, AxisMap>,
    /// X and Y are swapped (the `xpad` layout).
    pub swap_xy: bool,
    /// Triggers only as buttons (no analog trigger axes).
    pub digital_triggers: bool,
}

impl Mapping {
    /// The mapping for a device with these axes (code → (min, max)).
    pub fn new(abs: &BTreeMap<u16, (i32, i32)>, swap_xy: bool) -> Self {
        let mut axes = BTreeMap::new();
        for (&code, &(min, max)) in abs {
            let axis = match code {
                ABS_X => PadAxis::LeftX,
                ABS_Y => PadAxis::LeftY,
                ABS_RX => PadAxis::RightX,
                ABS_RY => PadAxis::RightY,
                ABS_Z | ABS_BRAKE => PadAxis::LeftTrigger,
                ABS_RZ | ABS_GAS => PadAxis::RightTrigger,
                ABS_HAT0X => PadAxis::DpadX,
                ABS_HAT0Y => PadAxis::DpadY,
                _ => continue,
            };
            if max > min {
                axes.insert(code, AxisMap { axis, min, max });
            }
        }
        let analog = |a| axes.values().any(|m: &AxisMap| m.axis == a);
        let digital_triggers = !analog(PadAxis::LeftTrigger) && !analog(PadAxis::RightTrigger);
        Self {
            axes,
            swap_xy,
            digital_triggers,
        }
    }
}

/// A device value scaled into the protocol axis's range.
pub fn scale(m: &AxisMap, value: i32) -> i32 {
    let r = m.axis.range();
    let (lo, hi) = (i64::from(*r.start()), i64::from(*r.end()));
    if m.axis == PadAxis::DpadX || m.axis == PadAxis::DpadY {
        return value.signum();
    }
    let v = i64::from(value.clamp(m.min, m.max));
    let span = i64::from(m.max) - i64::from(m.min);
    (lo + (v - i64::from(m.min)) * (hi - lo) / span) as i32
}

/// The state of one pad, to turn device events into protocol events and
/// to let go of everything when it is unplugged.
#[derive(Debug, Default)]
pub struct PadState {
    held: BTreeSet<u16>,
    /// Protocol axes away from rest.
    axes: BTreeMap<PadAxis, i32>,
    /// D-pad buttons held, for pads that report it as buttons.
    dpad: BTreeSet<u16>,
}

impl PadState {
    /// The protocol events for one device event (`kind`, `code`, `value`).
    pub fn translate(
        &mut self,
        map: &Mapping,
        pad: u8,
        kind: u16,
        code: u16,
        value: i32,
    ) -> Vec<InputEvent> {
        let mut out = Vec::new();
        let mut axis = |state: &mut Self, axis: PadAxis, v: i32| {
            if state.axes.get(&axis).copied().unwrap_or(0) == v {
                return;
            }
            if v == 0 {
                state.axes.remove(&axis);
            } else {
                state.axes.insert(axis, v);
            }
            out.push(InputEvent::PadAxis {
                pad,
                axis,
                value: v,
            });
        };
        match (kind, code) {
            (EV_ABS, _) => {
                if let Some(m) = map.axes.get(&code) {
                    axis(self, m.axis, scale(m, value));
                }
            }
            (EV_KEY, BTN_TL2 | BTN_TR2) => {
                if map.digital_triggers {
                    let a = if code == BTN_TL2 {
                        PadAxis::LeftTrigger
                    } else {
                        PadAxis::RightTrigger
                    };
                    axis(self, a, if value != 0 { 255 } else { 0 });
                }
            }
            (EV_KEY, BTN_DPAD_UP..=BTN_DPAD_RIGHT) => {
                if value != 0 {
                    self.dpad.insert(code);
                } else {
                    self.dpad.remove(&code);
                }
                let held = |c| i32::from(self.dpad.contains(&c));
                let x = held(BTN_DPAD_RIGHT) - held(BTN_DPAD_LEFT);
                let y = held(BTN_DPAD_DOWN) - held(BTN_DPAD_UP);
                axis(self, PadAxis::DpadX, x);
                axis(self, PadAxis::DpadY, y);
            }
            (EV_KEY, BTN_PAD_FIRST..=BTN_PAD_LAST) => {
                let code = match (map.swap_xy, code) {
                    (true, BTN_NORTH) => BTN_WEST,
                    (true, BTN_WEST) => BTN_NORTH,
                    _ => code,
                };
                // value 2 is the kernel's key repeat: not a change.
                let pressed = match value {
                    0 => false,
                    1 => true,
                    _ => return out,
                };
                if pressed == self.held.contains(&code) {
                    return out;
                }
                if pressed {
                    self.held.insert(code);
                } else {
                    self.held.remove(&code);
                }
                out.push(InputEvent::PadButton { pad, code, pressed });
            }
            _ => {}
        }
        out
    }

    /// Everything released and centered (the pad went away).
    pub fn release(&mut self, pad: u8) -> Vec<InputEvent> {
        let buttons =
            std::mem::take(&mut self.held)
                .into_iter()
                .map(|code| InputEvent::PadButton {
                    pad,
                    code,
                    pressed: false,
                });
        let axes = std::mem::take(&mut self.axes)
            .into_keys()
            .map(|axis| InputEvent::PadAxis {
                pad,
                axis,
                value: 0,
            });
        self.dpad.clear();
        buttons.chain(axes).collect()
    }
}

// ── evdev ───────────────────────────────────────────────────────────────

const fn ioc_read(nr: u64, size: u64) -> libc::c_ulong {
    ((2 << 30) | (size << 16) | ((b'E' as u64) << 8) | nr) as libc::c_ulong
}

fn ioctl_buf(file: &File, request: libc::c_ulong, buf: &mut [u8]) -> bool {
    // SAFETY: an evdev read ioctl into `buf`, whose size is encoded in the
    // request by the callers.
    unsafe { libc::ioctl(file.as_raw_fd(), request, buf.as_mut_ptr()) >= 0 }
}

fn bit(bits: &[u8], n: u16) -> bool {
    bits.get(usize::from(n / 8))
        .is_some_and(|b| b >> (n % 8) & 1 == 1)
}

/// What an opened event device is, if it is a gamepad.
struct Probe {
    name: String,
    vendor: u16,
    product: u16,
    abs: BTreeMap<u16, (i32, i32)>,
}

fn probe(file: &File) -> Option<Probe> {
    let mut name = [0u8; 256];
    ioctl_buf(file, ioc_read(0x06, 256), &mut name);
    let name = String::from_utf8_lossy(name.split(|&b| b == 0).next().unwrap_or(&[])).into_owned();
    let mut id = [0u8; 8];
    ioctl_buf(file, ioc_read(0x02, 8), &mut id);
    let mut keys = [0u8; 96];
    let mut abs_bits = [0u8; 8];
    if !ioctl_buf(file, ioc_read(0x20 + u64::from(EV_KEY), 96), &mut keys)
        || !ioctl_buf(file, ioc_read(0x20 + u64::from(EV_ABS), 8), &mut abs_bits)
    {
        return None;
    }
    if !bit(&keys, BTN_SOUTH) || !bit(&abs_bits, ABS_X) {
        return None;
    }
    let mut abs = BTreeMap::new();
    for code in 0..0x20u16 {
        if !bit(&abs_bits, code) {
            continue;
        }
        // struct input_absinfo: value, minimum, maximum, fuzz, flat, resolution.
        let mut info = [0u8; 24];
        if ioctl_buf(file, ioc_read(0x40 + u64::from(code), 24), &mut info) {
            let get = |i: usize| i32::from_ne_bytes(info[i..i + 4].try_into().expect("4 bytes"));
            abs.insert(code, (get(4), get(8)));
        }
    }
    Some(Probe {
        name,
        vendor: u16::from_ne_bytes([id[2], id[3]]),
        product: u16::from_ne_bytes([id[4], id[5]]),
        abs,
    })
}

/// Whether a device uses the `xpad` layout (X and Y swapped): driven by
/// `xpad`, or an Xbox 360 pad (like the host's virtual one).
fn xpad_layout(path: &Path, vendor: u16, product: u16) -> bool {
    let driver = path
        .file_name()
        .map(|n| {
            Path::new("/sys/class/input")
                .join(n)
                .join("device/device/driver")
        })
        .and_then(|d| std::fs::read_link(d).ok());
    driver.is_some_and(|d| d.ends_with("xpad")) || (vendor, product) == (0x045e, 0x028e)
}

struct OpenPad {
    path: PathBuf,
    file: File,
    slot: u8,
    map: Mapping,
    state: PadState,
}

/// Reads the machine's gamepads on a thread until dropped.
pub struct Gamepads {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Gamepads {
    /// Watches `dir` (`/dev/input`) and hands every pad event to `sink`.
    /// `skip_own`: leave out Fernsicht's own virtual devices.
    pub fn start(
        dir: PathBuf,
        skip_own: bool,
        mut sink: impl FnMut(InputEvent) + Send + 'static,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("gamepads".into())
            .spawn(move || {
                let mut pads: Vec<OpenPad> = Vec::new();
                let mut scanned: Option<Instant> = None;
                let mut buf = [0u8; 24 * 64];
                while !s.load(Ordering::Relaxed) {
                    if scanned.is_none_or(|t| t.elapsed() >= RESCAN) {
                        scan(&dir, skip_own, &mut pads);
                        scanned = Some(Instant::now());
                    }
                    if pads.is_empty() {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    let mut fds: Vec<libc::pollfd> = pads
                        .iter()
                        .map(|p| libc::pollfd {
                            fd: p.file.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        })
                        .collect();
                    // SAFETY: `fds` is a valid array of pollfd for the call.
                    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 100) };
                    let mut gone = Vec::new();
                    for (i, p) in pads.iter_mut().enumerate() {
                        if fds[i].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                            gone.push(i);
                            continue;
                        }
                        if fds[i].revents & libc::POLLIN == 0 {
                            continue;
                        }
                        match p.file.read(&mut buf) {
                            Ok(n) => {
                                for e in buf[..n].as_chunks::<24>().0 {
                                    let kind = u16::from_ne_bytes([e[16], e[17]]);
                                    let code = u16::from_ne_bytes([e[18], e[19]]);
                                    let value = i32::from_ne_bytes([e[20], e[21], e[22], e[23]]);
                                    for ev in p.state.translate(&p.map, p.slot, kind, code, value) {
                                        sink(ev);
                                    }
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(_) => gone.push(i),
                        }
                    }
                    for i in gone.into_iter().rev() {
                        let mut p = pads.remove(i);
                        log::info!("gamepad {} unplugged", p.slot + 1);
                        for ev in p.state.release(p.slot) {
                            sink(ev);
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

impl Drop for Gamepads {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Opens pads that appeared since the last look.
fn scan(dir: &Path, skip_own: bool, pads: &mut Vec<OpenPad>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
        })
        .collect();
    paths.sort();
    for path in paths {
        if pads.iter().any(|p| p.path == path) {
            continue;
        }
        let Some(slot) = (0..MAX_PADS).find(|s| pads.iter().all(|p| p.slot != *s)) else {
            return;
        };
        // No access (not a device of ours to read): skip quietly.
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
        else {
            continue;
        };
        let Some(probe) = probe(&file) else { continue };
        if skip_own && probe.name.starts_with("Fernsicht") {
            continue;
        }
        let map = Mapping::new(&probe.abs, xpad_layout(&path, probe.vendor, probe.product));
        log::info!("gamepad {}: {} ({})", slot + 1, probe.name, path.display());
        pads.push(OpenPad {
            path,
            file,
            slot,
            map,
            state: PadState::default(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Xbox-like pad: sticks ±32768, triggers 0..1023, hat.
    fn xbox(swap: bool) -> Mapping {
        let abs = [
            (ABS_X, (-32768, 32767)),
            (ABS_Y, (-32768, 32767)),
            (ABS_RX, (-32768, 32767)),
            (ABS_RY, (-32768, 32767)),
            (ABS_Z, (0, 1023)),
            (ABS_RZ, (0, 1023)),
            (ABS_HAT0X, (-1, 1)),
            (ABS_HAT0Y, (-1, 1)),
            (0x28, (0, 255)), // ABS_MISC: not ours
        ]
        .into_iter()
        .collect();
        Mapping::new(&abs, swap)
    }

    #[test]
    fn axes_are_scaled_to_the_protocol() {
        let m = xbox(false);
        assert_eq!(m.axes.len(), 8);
        assert!(!m.digital_triggers);
        let lx = m.axes[&ABS_X];
        assert_eq!(scale(&lx, -32768), -32768);
        assert_eq!(scale(&lx, 32767), 32767);
        let lt = m.axes[&ABS_Z];
        assert_eq!(scale(&lt, 1023), 255);
        assert_eq!(scale(&lt, 0), 0);
        assert_eq!(scale(&lt, 5000), 255, "clamped");
        // A PlayStation-like stick 0..255, centered at 128.
        let ps = AxisMap {
            axis: PadAxis::LeftX,
            min: 0,
            max: 255,
        };
        assert_eq!(scale(&ps, 0), -32768);
        assert_eq!(scale(&ps, 255), 32767);
        assert!(scale(&ps, 128).abs() < 200);
        let hat = m.axes[&ABS_HAT0Y];
        assert_eq!(scale(&hat, -1), -1);
        assert_eq!(scale(&hat, 1), 1);
        // A device axis with no range is left out.
        let flat = Mapping::new(&[(ABS_X, (0, 0))].into_iter().collect(), false);
        assert!(flat.axes.is_empty());
    }

    #[test]
    fn events_become_protocol_events() {
        let m = xbox(false);
        let mut s = PadState::default();
        let t = |s: &mut PadState, kind, code, value| s.translate(&m, 1, kind, code, value);
        assert_eq!(
            t(&mut s, EV_KEY, BTN_SOUTH, 1),
            [InputEvent::PadButton {
                pad: 1,
                code: BTN_SOUTH,
                pressed: true
            }]
        );
        assert!(t(&mut s, EV_KEY, BTN_SOUTH, 2).is_empty(), "key repeat");
        assert!(t(&mut s, EV_KEY, BTN_SOUTH, 1).is_empty(), "no change");
        assert_eq!(
            t(&mut s, EV_ABS, ABS_RZ, 1023),
            [InputEvent::PadAxis {
                pad: 1,
                axis: PadAxis::RightTrigger,
                value: 255
            }]
        );
        assert!(t(&mut s, EV_ABS, ABS_RZ, 1023).is_empty(), "no change");
        assert!(t(&mut s, EV_ABS, 0x28, 7).is_empty(), "unknown axis");
        assert!(t(&mut s, EV_KEY, 0x110, 1).is_empty(), "not a pad button");
        assert!(t(&mut s, 0x00, 0, 0).is_empty(), "SYN");
        // With analog triggers, the digital trigger buttons say nothing.
        assert!(t(&mut s, EV_KEY, BTN_TL2, 1).is_empty());
        // Unplugged: everything back.
        let mut released = s.release(1);
        released.sort_by_key(|e| format!("{e:?}"));
        assert_eq!(
            released,
            [
                InputEvent::PadAxis {
                    pad: 1,
                    axis: PadAxis::RightTrigger,
                    value: 0
                },
                InputEvent::PadButton {
                    pad: 1,
                    code: BTN_SOUTH,
                    pressed: false
                },
            ]
        );
        assert!(s.release(1).is_empty());
    }

    #[test]
    fn xpad_layout_is_turned_into_positions() {
        let m = xbox(true);
        let mut s = PadState::default();
        assert_eq!(
            s.translate(&m, 0, EV_KEY, BTN_NORTH, 1),
            [InputEvent::PadButton {
                pad: 0,
                code: BTN_WEST,
                pressed: true
            }]
        );
        assert_eq!(
            s.translate(&m, 0, EV_KEY, BTN_WEST, 1),
            [InputEvent::PadButton {
                pad: 0,
                code: BTN_NORTH,
                pressed: true
            }]
        );
    }

    #[test]
    fn dpad_buttons_and_digital_triggers_become_axes() {
        // Only sticks: triggers and d-pad are buttons.
        let m = Mapping::new(
            &[(ABS_X, (0, 255)), (ABS_Y, (0, 255))].into_iter().collect(),
            false,
        );
        assert!(m.digital_triggers);
        let mut s = PadState::default();
        let axis = |axis, value| InputEvent::PadAxis {
            pad: 0,
            axis,
            value,
        };
        assert_eq!(
            s.translate(&m, 0, EV_KEY, BTN_TR2, 1),
            [axis(PadAxis::RightTrigger, 255)]
        );
        assert_eq!(
            s.translate(&m, 0, EV_KEY, BTN_DPAD_LEFT, 1),
            [axis(PadAxis::DpadX, -1)]
        );
        assert_eq!(
            s.translate(&m, 0, EV_KEY, BTN_DPAD_DOWN, 1),
            [axis(PadAxis::DpadY, 1)]
        );
        assert_eq!(
            s.translate(&m, 0, EV_KEY, BTN_DPAD_LEFT, 0),
            [axis(PadAxis::DpadX, 0)]
        );
        assert_eq!(
            s.translate(&m, 0, EV_KEY, BTN_TR2, 0),
            [axis(PadAxis::RightTrigger, 0)]
        );
    }

    #[test]
    fn bits_and_ioctl_numbers() {
        assert!(bit(&[0b10], 1));
        assert!(!bit(&[0b10], 0));
        assert!(!bit(&[], 9));
        // EVIOCGNAME(256) and EVIOCGABS(0) as the kernel headers spell them.
        assert_eq!(ioc_read(0x06, 256), 0x8100_4506);
        assert_eq!(ioc_read(0x40, 24), 0x8018_4540);
    }

    #[test]
    fn a_directory_without_pads_is_fine() {
        let dir = std::env::temp_dir().join(format!("fernsicht-pads-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("event0"), b"not a device").unwrap();
        let mut pads = Vec::new();
        scan(&dir, true, &mut pads);
        assert!(pads.is_empty());
        scan(Path::new("/does/not/exist"), true, &mut pads);
        let g = Gamepads::start(dir.clone(), true, |_| {}).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        drop(g);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
