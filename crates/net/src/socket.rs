//! UDP sockets with large kernel buffers.
//!
//! A keyframe can be a few hundred kilobytes that arrive within a couple of
//! milliseconds. With the Linux default receive buffer (~208 KiB) the kernel
//! silently drops the tail of such a burst whenever the receiving thread is
//! briefly descheduled (`RcvbufErrors` in `/proc/net/snmp`). The kernel caps
//! the request at `net.core.rmem_max` / `wmem_max`; raise those with
//! `sysctl` if the warning below shows up.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::os::fd::AsRawFd;

use socket2::{Domain, Protocol, Socket, Type};

/// Requested send and receive buffer size.
pub const BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Binds a UDP socket with [`BUFFER_BYTES`] send/receive buffers.
pub fn bind_udp(addr: impl ToSocketAddrs) -> io::Result<UdpSocket> {
    let addr: SocketAddr = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address"))?;
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    // Past rmem_max/wmem_max where allowed (the host service has
    // CAP_NET_ADMIN); otherwise best effort, capped by them.
    if !force_buffer(&socket, libc::SO_RCVBUFFORCE) {
        let _ = socket.set_recv_buffer_size(BUFFER_BYTES);
    }
    if !force_buffer(&socket, libc::SO_SNDBUFFORCE) {
        let _ = socket.set_send_buffer_size(BUFFER_BYTES);
    }
    socket.bind(&addr.into())?;
    if let Ok(reported) = socket.recv_buffer_size()
        && usable(reported) < BUFFER_BYTES
    {
        log::warn!(
            "UDP receive buffer is {} KiB (wanted {} KiB); raise net.core.rmem_max to avoid drops on keyframes",
            usable(reported) / 1024,
            BUFFER_BYTES / 1024
        );
    }
    Ok(socket.into())
}

/// The usable part of a buffer size Linux reports: it doubles what was
/// asked for (bookkeeping overhead), and reports the doubled value.
pub fn usable(reported: usize) -> usize {
    reported / 2
}

/// Sets a buffer of [`BUFFER_BYTES`] with `SO_RCVBUFFORCE` or
/// `SO_SNDBUFFORCE`, which ignore the system maximum but need
/// CAP_NET_ADMIN; false without it.
fn force_buffer(socket: &Socket, option: libc::c_int) -> bool {
    let bytes = BUFFER_BYTES as libc::c_int;
    // SAFETY: a valid socket; the option value is a c_int of that size.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&bytes as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
    }
}

/// A socket of `peer`'s family on any local address and port, to talk to
/// `peer` (a v4 socket cannot reach a v6 address).
pub fn local_for(peer: &SocketAddr) -> SocketAddr {
    match peer {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    }
}

/// A UDP socket ([`bind_udp`]) connected to `host` (`name:port`,
/// `1.2.3.4:port` or `[v6]:port`), of the address family `host` resolves
/// to.
pub fn connect_udp(host: &str) -> io::Result<UdpSocket> {
    let peer = host
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{host}: no address")))?;
    let socket = bind_udp(local_for(&peer))?;
    socket.connect(peer)?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usable_buffer_is_half_what_linux_reports() {
        // Asked for 4 MiB and got it: Linux reports 8 MiB.
        assert_eq!(usable(8 * 1024 * 1024), BUFFER_BYTES);
        // Capped at an rmem_max of 2 MiB: reported 4 MiB, half usable.
        assert!(usable(4 * 1024 * 1024) < BUFFER_BYTES);
    }

    #[test]
    fn the_local_side_takes_the_peers_family() {
        let v4: SocketAddr = "192.0.2.1:47800".parse().unwrap();
        let v6: SocketAddr = "[fe80::1]:47800".parse().unwrap();
        assert_eq!(local_for(&v4), "0.0.0.0:0".parse().unwrap());
        assert_eq!(local_for(&v6), "[::]:0".parse().unwrap());
    }

    #[test]
    fn talks_to_a_v6_host() {
        // Machines without IPv6 (some containers) have no ::1.
        let Ok(host) = UdpSocket::bind("[::1]:0") else {
            return;
        };
        let client = connect_udp(&host.local_addr().unwrap().to_string()).unwrap();
        client.send(b"hi").unwrap();
        let mut buf = [0u8; 8];
        let (n, from) = host.recv_from(&mut buf).unwrap();
        assert_eq!(
            (&buf[..n], from),
            (&b"hi"[..], client.local_addr().unwrap())
        );
    }

    #[test]
    fn binds_and_talks() {
        let a = bind_udp("127.0.0.1:0").unwrap();
        let b = bind_udp("127.0.0.1:0").unwrap();
        a.send_to(b"hi", b.local_addr().unwrap()).unwrap();
        let mut buf = [0u8; 8];
        let (n, from) = b.recv_from(&mut buf).unwrap();
        assert_eq!((&buf[..n], from), (&b"hi"[..], a.local_addr().unwrap()));
    }
}
