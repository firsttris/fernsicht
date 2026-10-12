//! A virtual DualSense through `/dev/uhid` (D9): the kernel's
//! hid-playstation driver binds to it as to Sony's pad and makes its
//! gamepad, motion sensor and touchpad devices; Steam and SDL read its
//! hidraw node. The reports are [`crate::dualsense`]'s; here are the uhid
//! events that carry them (`include/uapi/linux/uhid.h`) and the thread
//! that answers the kernel.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use fernsicht_proto::InputEvent;

use crate::RumbleOut;
use crate::dualsense::{self, State};

// enum uhid_event_type.
const UHID_DESTROY: u32 = 1;
const UHID_OUTPUT: u32 = 6;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;
/// `UHID_DATA_MAX`, `HID_MAX_DESCRIPTOR_SIZE`.
const DATA_MAX: usize = 4096;
/// `sizeof(struct uhid_event)`: the type and its largest member,
/// `uhid_create2_req`.
const EVENT_LEN: usize = 4 + 128 + 64 + 64 + 2 + 2 + 4 * 4 + DATA_MAX;
const BUS_USB: u16 = 0x03;
const EINVAL: u16 = 22;

/// `UHID_CREATE2` for a DualSense named `name`, with `uniq` (its MAC as
/// text) and the USB report descriptor.
fn create2(name: &str, phys: &str, uniq: &str) -> Vec<u8> {
    let mut e = vec![0u8; EVENT_LEN];
    e[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
    let text = |e: &mut [u8], at: usize, len: usize, s: &str| {
        let b = s.as_bytes();
        let n = b.len().min(len - 1);
        e[at..at + n].copy_from_slice(&b[..n]);
    };
    text(&mut e, 4, 128, name);
    text(&mut e, 132, 64, phys);
    text(&mut e, 196, 64, uniq);
    let d = &dualsense::DESCRIPTOR;
    e[260..262].copy_from_slice(&(d.len() as u16).to_ne_bytes());
    e[262..264].copy_from_slice(&BUS_USB.to_ne_bytes());
    e[264..268].copy_from_slice(&dualsense::VENDOR.to_ne_bytes());
    e[268..272].copy_from_slice(&dualsense::PRODUCT.to_ne_bytes());
    // version 0x8111 as the pad's USB descriptor has it, country 0.
    e[272..276].copy_from_slice(&0x8111u32.to_ne_bytes());
    e[280..280 + d.len()].copy_from_slice(d);
    e
}

/// `UHID_INPUT2` with `report`; only as long as the report (the kernel
/// fills the rest with zeros).
fn input2(report: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(6 + report.len());
    e.extend_from_slice(&UHID_INPUT2.to_ne_bytes());
    e.extend_from_slice(&(report.len() as u16).to_ne_bytes());
    e.extend_from_slice(report);
    e
}

/// The answer to request `id`: `data`, or `EINVAL` without.
fn get_report_reply(id: u32, data: Option<&[u8]>) -> Vec<u8> {
    let data = data.unwrap_or(&[]);
    let mut e = Vec::with_capacity(12 + data.len());
    e.extend_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
    e.extend_from_slice(&id.to_ne_bytes());
    let err = if data.is_empty() { EINVAL } else { 0 };
    e.extend_from_slice(&err.to_ne_bytes());
    e.extend_from_slice(&(data.len() as u16).to_ne_bytes());
    e.extend_from_slice(data);
    e
}

fn set_report_reply(id: u32) -> Vec<u8> {
    let mut e = Vec::with_capacity(10);
    e.extend_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
    e.extend_from_slice(&id.to_ne_bytes());
    e.extend_from_slice(&0u16.to_ne_bytes());
    e
}

/// What the kernel asks of the device.
#[derive(Debug, PartialEq, Eq)]
enum Request<'a> {
    /// A feature report (`id` to answer with).
    GetReport {
        id: u32,
        report: u8,
    },
    /// A report written to it (rumble, lightbar).
    Output(&'a [u8]),
    /// A feature report written to it: acknowledged, not used.
    SetReport {
        id: u32,
    },
    Other,
}

fn request(e: &[u8]) -> Request<'_> {
    let Some(kind) = e
        .get(..4)
        .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
    else {
        return Request::Other;
    };
    let u32_at = |at: usize| {
        e.get(at..at + 4)
            .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
    };
    match kind {
        UHID_GET_REPORT => match (u32_at(4), e.get(8)) {
            (Some(id), Some(&report)) => Request::GetReport { id, report },
            _ => Request::Other,
        },
        UHID_OUTPUT => {
            let size = e
                .get(4 + DATA_MAX..6 + DATA_MAX)
                .map_or(0, |b| usize::from(u16::from_ne_bytes([b[0], b[1]])));
            e.get(4..4 + size.min(DATA_MAX))
                .map_or(Request::Other, Request::Output)
        }
        UHID_SET_REPORT => u32_at(4).map_or(Request::Other, |id| Request::SetReport { id }),
        _ => Request::Other,
    }
}

/// The answer to a request, if it takes one; a rumble to pass on.
fn answer(pad: u8, mac: [u8; 6], req: &Request<'_>) -> (Option<Vec<u8>>, Option<crate::Rumble>) {
    match *req {
        Request::GetReport { id, report } => (
            Some(get_report_reply(
                id,
                dualsense::feature(report, mac).as_deref(),
            )),
            None,
        ),
        Request::SetReport { id } => (Some(set_report_reply(id)), None),
        Request::Output(data) => (None, dualsense::rumble(pad, data)),
        Request::Other => (None, None),
    }
}

/// A locally administered address, different per pad and host process
/// (the kernel names the pad's devices by it).
pub fn mac_of(pad: u8) -> [u8; 6] {
    let p = std::process::id().to_le_bytes();
    [0x02, 0x46, p[0], p[1], p[2], pad]
}

fn mac_text(mac: [u8; 6]) -> String {
    // The kernel prints it reversed (`%pMR` of the pairing bytes).
    let m: Vec<String> = mac.iter().rev().map(|b| format!("{b:02x}")).collect();
    m.join(":")
}

/// A DualSense on `/dev/uhid`, gone when dropped.
pub struct DualSense {
    file: File,
    state: State,
    seq: u8,
    since: Instant,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl DualSense {
    /// Creates pad `pad` (0-based); what games make it rumble goes to
    /// `rumble`.
    pub fn create(path: &Path, pad: u8, rumble: Option<RumbleOut>) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mac = mac_of(pad);
        let name = format!("Fernsicht DualSense pad {}", pad + 1);
        (&file)
            .write_all(&create2(
                &name,
                &format!("fernsicht/pad{}", pad + 1),
                &mac_text(mac),
            ))
            .map_err(|e| format!("uhid create: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let reader = file.try_clone().map_err(|e| e.to_string())?;
        let s = stop.clone();
        // The kernel's driver asks for feature reports while it binds (in
        // the write above's aftermath), so the answers must come from
        // another thread than the one that waits for the device.
        let thread = std::thread::Builder::new()
            .name(format!("dualsense {}", pad + 1))
            .spawn(move || serve(reader, pad, mac, rumble, &s))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            file,
            state: State::default(),
            seq: 0,
            since: Instant::now(),
            stop,
            thread: Some(thread),
        })
    }

    /// Takes one of the pad's events; sends a report when it changed
    /// something (one write).
    pub fn apply(&mut self, event: &InputEvent) -> Result<(), String> {
        if self.state.apply(event) {
            self.send()?;
        }
        Ok(())
    }

    /// Buttons up, sticks centred, fingers lifted.
    pub fn release(&mut self) -> Result<(), String> {
        let rest = self.state.released();
        if rest != self.state {
            self.state = rest;
            self.send()?;
        }
        Ok(())
    }

    fn send(&mut self) -> Result<(), String> {
        self.seq = self.seq.wrapping_add(1);
        // The sensors' clock, in 1/3 µs (hid-playstation.c).
        let ts = (self.since.elapsed().as_nanos() / 333) as u32;
        let report = dualsense::input_report(&self.state, self.seq, ts);
        (&self.file)
            .write_all(&input2(&report))
            .map_err(|e| format!("uhid input: {e}"))
    }
}

impl Drop for DualSense {
    fn drop(&mut self) {
        let _ = (&self.file).write_all(&UHID_DESTROY.to_ne_bytes());
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Answers the kernel until `stop`.
fn serve(mut file: File, pad: u8, mac: [u8; 6], rumble: Option<RumbleOut>, stop: &AtomicBool) {
    let mut buf = vec![0u8; EVENT_LEN];
    while !stop.load(Ordering::Relaxed) {
        let mut fd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut fd, 1, 100) } <= 0 {
            continue;
        }
        let Ok(n) = file.read(&mut buf) else {
            continue;
        };
        let req = request(&buf[..n]);
        let (reply, played) = answer(pad, mac, &req);
        if let Some(reply) = reply
            && let Err(e) = file.write_all(&reply)
        {
            log::warn!("dualsense {}: answering the kernel: {e}", pad + 1);
        }
        if let (Some(r), Some(out)) = (played, &rumble) {
            out(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_have_the_kernels_layout() {
        // sizeof(struct uhid_event) with uhid_create2_req the largest.
        assert_eq!(EVENT_LEN, 4376);
        let c = create2("Pad", "p", "02:46");
        assert_eq!(c.len(), EVENT_LEN);
        assert_eq!(&c[4..7], b"Pad");
        assert_eq!(c[132], b'p');
        assert_eq!(&c[196..201], b"02:46");
        assert_eq!(u16::from_ne_bytes([c[260], c[261]]), 273);
        assert_eq!(u16::from_ne_bytes([c[262], c[263]]), BUS_USB);
        assert_eq!(u32::from_ne_bytes(c[264..268].try_into().unwrap()), 0x054c);
        assert_eq!(c[280..282], [0x05, 0x01]);
        let long = create2(&"n".repeat(200), "", "");
        assert_eq!(long[4 + 127], 0, "names end with a NUL");
        let i = input2(&[1, 2, 3]);
        assert_eq!(i, [12, 0, 0, 0, 3, 0, 1, 2, 3]);
    }

    #[test]
    fn requests_are_read_and_answered() {
        let mut get = vec![0u8; 10];
        get[0..4].copy_from_slice(&UHID_GET_REPORT.to_ne_bytes());
        get[4..8].copy_from_slice(&77u32.to_ne_bytes());
        get[8] = 0x05;
        let req = request(&get);
        assert_eq!(
            req,
            Request::GetReport {
                id: 77,
                report: 0x05
            }
        );
        let (reply, rumble) = answer(0, mac_of(0), &req);
        let reply = reply.unwrap();
        assert_eq!(
            u32::from_ne_bytes(reply[0..4].try_into().unwrap()),
            UHID_GET_REPORT_REPLY
        );
        assert_eq!(u32::from_ne_bytes(reply[4..8].try_into().unwrap()), 77);
        assert_eq!(u16::from_ne_bytes([reply[8], reply[9]]), 0, "no error");
        assert_eq!(u16::from_ne_bytes([reply[10], reply[11]]), 41);
        assert_eq!(reply[12], 0x05);
        assert!(rumble.is_none());
        // A report the pad does not have.
        get[8] = 0x83;
        let (reply, _) = answer(0, mac_of(0), &request(&get));
        let reply = reply.unwrap();
        assert_eq!(u16::from_ne_bytes([reply[8], reply[9]]), EINVAL);

        let mut out = vec![0u8; EVENT_LEN];
        out[0..4].copy_from_slice(&UHID_OUTPUT.to_ne_bytes());
        out[4] = 0x02;
        out[5] = 0x01;
        out[8] = 0xff;
        out[4 + DATA_MAX..6 + DATA_MAX].copy_from_slice(&48u16.to_ne_bytes());
        let req = request(&out);
        assert_eq!(req, Request::Output(&out[4..52]));
        let (reply, rumble) = answer(2, mac_of(2), &req);
        assert!(reply.is_none());
        assert_eq!(rumble.map(|r| (r.pad, r.low)), Some((2, 0xffff)));

        let mut set = vec![0u8; 12];
        set[0..4].copy_from_slice(&UHID_SET_REPORT.to_ne_bytes());
        set[4..8].copy_from_slice(&5u32.to_ne_bytes());
        assert_eq!(request(&set), Request::SetReport { id: 5 });
        assert_eq!(request(&[1, 0]), Request::Other, "short");
    }

    #[test]
    fn pads_have_their_own_address() {
        assert_ne!(mac_of(0), mac_of(1));
        assert_eq!(mac_of(0)[0] & 0x03, 0x02, "locally administered, unicast");
        assert_eq!(mac_text([1, 2, 3, 4, 5, 0xab]), "ab:05:04:03:02:01");
    }

    #[test]
    fn a_missing_device_node_is_a_clear_error() {
        let err = DualSense::create(Path::new("/nonexistent/uhid"), 0, None)
            .err()
            .unwrap();
        assert!(err.contains("/nonexistent/uhid"), "{err}");
    }
}
