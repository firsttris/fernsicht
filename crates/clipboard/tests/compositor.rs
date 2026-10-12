//! The data-control client against a real compositor: two clients on one
//! seat copy to each other. Runs where `FERNSICHT_WAYLAND_SOCKET` names a
//! compositor's socket that offers data control (a headless Sway in CI,
//! `kwin_wayland --virtual` by hand), else it skips.

use std::path::PathBuf;
use std::time::Duration;

use fernsicht_clipboard::{Clipboard, Current};

fn socket() -> Option<PathBuf> {
    let s = std::env::var_os("FERNSICHT_WAYLAND_SOCKET").map(PathBuf::from);
    if s.is_none() {
        eprintln!("skipped: set FERNSICHT_WAYLAND_SOCKET (a compositor with data control)");
    }
    s
}

/// The next text `c` sees within two seconds.
fn next(c: &Clipboard) -> Option<String> {
    c.changes().recv_timeout(Duration::from_secs(2)).ok()
}

/// Texts that arrive within a moment (none expected).
fn quiet(c: &Clipboard) -> Vec<String> {
    std::thread::sleep(Duration::from_millis(300));
    c.changes().try_iter().collect()
}

#[test]
fn two_clients_copy_to_each_other_and_not_to_themselves() {
    let Some(socket) = socket() else { return };
    let a = Clipboard::at(&socket, Current::Report).expect("data control");
    let b = Clipboard::at(&socket, Current::Report).expect("data control");
    // Whatever the clipboard held before.
    let _ = quiet(&a);
    let _ = quiet(&b);

    a.set("Grüße vom Host".into());
    assert_eq!(next(&b).as_deref(), Some("Grüße vom Host"));
    assert!(quiet(&a).is_empty(), "its own copy is not read back");

    b.set("zurück".into());
    assert_eq!(next(&a).as_deref(), Some("zurück"));

    // As long as goes across; written and read on threads of their own.
    let long = "0123456789".repeat(20_000);
    a.set(long.clone());
    assert_eq!(next(&b).map(|t| t.len()), Some(long.len()));

    // Opened with what is there: reported (the client), or not (the host).
    let reporting = Clipboard::at(&socket, Current::Report).expect("data control");
    assert_eq!(next(&reporting).map(|t| t.len()), Some(long.len()));
    let skipping = Clipboard::at(&socket, Current::Skip).expect("data control");
    assert!(quiet(&skipping).is_empty());
}
