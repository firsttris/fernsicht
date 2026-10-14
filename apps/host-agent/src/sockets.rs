//! The host's UDP sockets: the one it is told to bind, and beside an IPv4
//! one on all addresses an IPv6 one on the same port, so that clients can
//! reach it over IPv6 too (the direct path for connections over the
//! internet where IPv4 sits behind a carrier's NAT, `docs/internet.md`).

use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use fernsicht_net::socket;

pub(crate) struct Sockets {
    v4: Arc<UdpSocket>,
    /// `None` when the host binds one address (tests, `--bind`), or IPv6
    /// is not there.
    v6: Option<Arc<UdpSocket>>,
}

impl Sockets {
    /// Binds `addr`; when that is IPv4's “any address”, IPv6's too on the
    /// same port (best effort: a machine without IPv6 goes on without).
    pub(crate) fn bind(addr: &str) -> std::io::Result<Self> {
        let first = socket::bind_udp(addr)?;
        let local = first.local_addr()?;
        let v6 = match local {
            SocketAddr::V4(v4) if v4.ip().is_unspecified() => {
                match socket::bind_udp_v6_only(("::", local.port())) {
                    Ok(s) => Some(Arc::new(s)),
                    Err(e) => {
                        log::info!("no IPv6 on port {} ({e}): IPv4 only", local.port());
                        None
                    }
                }
            }
            _ => None,
        };
        Ok(Self {
            v4: Arc::new(first),
            v6,
        })
    }

    fn all(&self) -> impl Iterator<Item = &Arc<UdpSocket>> {
        std::iter::once(&self.v4).chain(self.v6.as_ref())
    }

    /// The socket packets to `peer` go out on.
    pub(crate) fn for_peer(&self, peer: SocketAddr) -> &Arc<UdpSocket> {
        match (peer, &self.v6) {
            (SocketAddr::V6(_), Some(v6)) => v6,
            _ => &self.v4,
        }
    }

    pub(crate) fn send_to(&self, buf: &[u8], to: SocketAddr) -> std::io::Result<usize> {
        self.for_peer(to).send_to(buf, to)
    }

    /// Whether the host listens on IPv6 too.
    pub(crate) fn has_v6(&self) -> bool {
        self.v6.is_some() || self.v4.local_addr().is_ok_and(|a| a.is_ipv6())
    }

    /// The first socket's address (its port is the host's port).
    pub(crate) fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.v4.local_addr()
    }

    pub(crate) fn set_dscp(&self, dscp: socket::Dscp) -> std::io::Result<()> {
        self.all().try_for_each(|s| socket::set_dscp(s, dscp))
    }

    /// The next datagram on any of the sockets, waiting up to `wait`.
    pub(crate) fn recv_from(
        &self,
        buf: &mut [u8],
        wait: Duration,
    ) -> std::io::Result<Option<(usize, SocketAddr)>> {
        let mut fds: Vec<libc::pollfd> = self
            .all()
            .map(|s| libc::pollfd {
                fd: s.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let ms = wait.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: `fds` is a valid array of pollfd of the length given.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return if e.kind() == std::io::ErrorKind::Interrupted {
                Ok(None)
            } else {
                Err(e)
            };
        }
        // Only this thread reads, so a readable socket does not block.
        let ready = self.all().zip(&fds).find(|(_, p)| p.revents != 0);
        match ready {
            Some((s, _)) => s.recv_from(buf).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_families_on_one_port() {
        let s = Sockets::bind("0.0.0.0:0").unwrap();
        let port = s.local_addr().unwrap().port();
        let Some(v6) = &s.v6 else {
            // A machine without IPv6: IPv4 alone works as before.
            return;
        };
        assert_eq!(v6.local_addr().unwrap().port(), port);
        let mut buf = [0u8; 16];
        for to in [format!("127.0.0.1:{port}"), format!("[::1]:{port}")] {
            let peer = UdpSocket::bind(if to.starts_with('[') {
                "[::1]:0"
            } else {
                "127.0.0.1:0"
            })
            .unwrap();
            peer.send_to(b"hi", &to).unwrap();
            let (n, from) = s
                .recv_from(&mut buf, Duration::from_secs(1))
                .unwrap()
                .unwrap();
            assert_eq!(&buf[..n], b"hi");
            assert_eq!(from, peer.local_addr().unwrap());
            // The answer goes out on the socket of the peer's family.
            s.send_to(b"yo", from).unwrap();
            let mut back = [0u8; 4];
            let (n, _) = peer.recv_from(&mut back).unwrap();
            assert_eq!(&back[..n], b"yo");
        }
        assert!(
            s.recv_from(&mut buf, Duration::from_millis(10))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn one_address_binds_one_socket() {
        let s = Sockets::bind("127.0.0.1:0").unwrap();
        assert!(s.v6.is_none());
        assert!(Arc::ptr_eq(s.for_peer("[::1]:1".parse().unwrap()), &s.v4));
    }
}
