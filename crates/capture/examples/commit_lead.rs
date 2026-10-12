//! How long before the vblank a compositor's new picture can be seen on
//! the plane (D8: capture on commit instead of at the vblank).
//!
//! A thread reads the primary plane's framebuffer every 200 µs; the main
//! thread waits for each vblank of its CRTC. For every new framebuffer the
//! lead is the time from seeing it to the next vblank. A lead close to a
//! whole refresh period means the plane only shows the buffer once it is
//! flipped (nothing to win); a lead of a few ms means the commit is
//! visible before the vblank.
//!
//! Needs no privileges (reading plane state and waiting for vblanks are
//! allowed to every client of the card); the screen has to change.
//!
//!   cargo run -p fernsicht-capture --features kms --example commit_lead -- [card] [seconds]

use std::fs::OpenOptions;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use drm::control::{Device as _, plane};
use drm::{ClientCapability, Device as _, VblankWaitFlags, VblankWaitTarget};

struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl drm::control::Device for Card {}

fn open(path: &str) -> Card {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"));
    let card = Card(file);
    card.set_client_capability(ClientCapability::UniversalPlanes, true)
        .expect("universal planes");
    let _ = card.set_client_capability(ClientCapability::Atomic, true);
    card
}

/// The first plane showing a framebuffer on an active CRTC, with the CRTC's
/// index (the vblank pipe) and refresh rate.
fn first_shown_plane(card: &Card) -> Option<(plane::Handle, u32, u32)> {
    let res = card.resource_handles().ok()?;
    card.plane_handles().ok()?.into_iter().find_map(|h| {
        let info = card.get_plane(h).ok()?;
        let (crtc, _) = (info.crtc()?, info.framebuffer()?);
        let index = res.crtcs().iter().position(|&c| c == crtc)? as u32;
        let hz = card.get_crtc(crtc).ok()?.mode()?.vrefresh();
        Some((h, index, hz))
    })
}

fn monotonic_to_instant(t: Duration) -> Instant {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    let age = Duration::new(now.tv_sec as u64, now.tv_nsec as u32).saturating_sub(t);
    Instant::now().checked_sub(age).unwrap_or_else(Instant::now)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "/dev/dri/card1".into());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let card = open(&path);
    let (plane, pipe, hz) = first_shown_plane(&card).expect("no plane shows a framebuffer");
    println!("{path}: plane {plane:?}, pipe {pipe}, {hz} Hz, {secs} s");
    let end = Instant::now() + Duration::from_secs(secs);

    let (tx, rx) = mpsc::channel::<(Instant, u32)>();
    let poller = std::thread::spawn({
        let card = open(&path);
        move || {
            let mut last = None;
            while Instant::now() < end {
                let fb = card
                    .get_plane(plane)
                    .ok()
                    .and_then(|p| p.framebuffer())
                    .map(u32::from);
                if fb.is_some() && fb != last {
                    last = fb;
                    let _ = tx.send((Instant::now(), fb.unwrap_or(0)));
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        }
    });
    let mut vblanks = Vec::new();
    while Instant::now() < end {
        match card.wait_vblank(
            VblankWaitTarget::Relative(1),
            VblankWaitFlags::empty(),
            pipe,
            0,
        ) {
            Ok(r) => vblanks.push(r.time().map_or_else(Instant::now, monotonic_to_instant)),
            Err(e) => panic!("vblank: {e}"),
        }
    }
    poller.join().unwrap();
    let changes: Vec<(Instant, u32)> = rx.try_iter().skip(1).collect();
    let mut leads: Vec<f64> = changes
        .iter()
        .filter_map(|(t, _)| vblanks.iter().find(|v| *v > t))
        .zip(&changes)
        .map(|(v, (t, _))| v.duration_since(*t).as_secs_f64() * 1e3)
        .collect();
    leads.sort_by(f64::total_cmp);
    let period = 1000.0 / f64::from(hz);
    println!(
        "{} vblanks, {} new framebuffers (refresh period {period:.2} ms)",
        vblanks.len(),
        leads.len()
    );
    if leads.is_empty() {
        println!("nothing changed on the screen");
        return;
    }
    println!(
        "lead to the next vblank: min {:.2} · p10 {:.2} · median {:.2} · p90 {:.2} · max {:.2} ms",
        leads[0],
        percentile(&leads, 0.1),
        percentile(&leads, 0.5),
        percentile(&leads, 0.9),
        leads[leads.len() - 1]
    );
}
