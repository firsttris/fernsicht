//! The clipboard between client and host (text).
//!
//! - [`wayland`]: a Wayland desktop's clipboard through data control, the
//!   protocol for clipboard managers (no window, no focus needed).
//! - [`transfer`]: a text across the lossy link, in chunks with
//!   acknowledgements.

pub mod transfer;
pub mod wayland;

pub use transfer::{CHUNK, Incoming, MAX_TEXT, Outgoing, Received, Reply, Sync};
pub use wayland::{Clipboard, Current};

/// Whose clipboard a session shares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Place {
    /// None (`--no-clipboard`).
    Off,
    /// This machine's: the client's own session, the host's desktop user.
    #[default]
    Here,
    /// The compositor on this socket (tests).
    At(std::path::PathBuf),
}

/// The name of a desktop's compositor socket among the names in its
/// runtime directory: `wayland-0` if there, else the lowest `wayland-N`
/// (not the `.lock` files).
pub fn wayland_socket<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    names
        .into_iter()
        .filter_map(|n| Some((n.strip_prefix("wayland-")?.parse::<u32>().ok()?, n)))
        .min()
        .map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_desktops_socket_is_found_among_its_files() {
        use super::wayland_socket;
        let names = ["pulse", "wayland-1", "wayland-0.lock", "wayland-0", "bus"];
        assert_eq!(wayland_socket(names), Some("wayland-0"));
        assert_eq!(
            wayland_socket(["wayland-2", "wayland-1.lock"]),
            Some("wayland-2")
        );
        assert_eq!(wayland_socket(["pulse", "wayland-x"]), None);
    }
}
