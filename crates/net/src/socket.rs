//! UDP sockets with large kernel buffers, and bursts of equal datagrams in
//! one system call each way (UDP GSO to send, GRO to receive).
//!
//! A keyframe can be a few hundred kilobytes that arrive within a couple of
//! milliseconds. With the Linux default receive buffer (~208 KiB) the kernel
//! silently drops the tail of such a burst whenever the receiving thread is
//! briefly descheduled (`RcvbufErrors` in `/proc/net/snmp`). The kernel caps
//! the request at `net.core.rmem_max` / `wmem_max`; raise those with
//! `sysctl` if the warning below shows up.

use std::io::{self, IoSlice};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::os::fd::AsRawFd;

use socket2::{Domain, Protocol, Socket, Type};

/// Datagrams one [`send_segments`] call may carry (the kernel's
/// `UDP_MAX_SEGMENTS`).
pub const MAX_SEGMENTS: usize = 64;

/// Bytes one [`send_segments`] call may carry: an IP packet holds 65,507,
/// and the chunks the pacer forms grow by sealing.
pub const MAX_SEGMENTS_BYTES: usize = 60_000;

/// Sends `bufs` to `to` as datagrams of `segment` bytes each (the last may
/// be shorter) in one system call: the kernel cuts them (UDP GSO, Linux
/// 4.18). At most [`MAX_SEGMENTS`] datagrams and [`MAX_SEGMENTS_BYTES`];
/// the buffers' boundaries do not matter, only `segment` does. Fails where
/// the kernel or the route cannot do it; then send one by one.
pub fn send_segments(
    socket: &UdpSocket,
    to: SocketAddr,
    bufs: &[IoSlice<'_>],
    segment: u16,
) -> io::Result<usize> {
    #[repr(C, align(8))]
    struct Control([u8; 32]);
    let mut control = Control([0; 32]);
    // SAFETY: the CMSG helpers on a zeroed, aligned buffer with room for
    // one header and a u16 (CMSG_SPACE(2) is 24 on 64-bit Linux).
    let len = unsafe {
        let space = libc::CMSG_SPACE(2) as usize;
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_control = control.0.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_UDP;
        (*cmsg).cmsg_type = libc::UDP_SEGMENT;
        (*cmsg).cmsg_len = libc::CMSG_LEN(2) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<u16>(), segment);
        space
    };
    let addr = socket2::SockAddr::from(to);
    let msg = socket2::MsgHdr::new()
        .with_addr(&addr)
        .with_buffers(bufs)
        .with_control(&control.0[..len]);
    socket2::SockRef::from(socket).sendmsg(&msg, 0)
}

/// Lets the kernel hand over runs of equal datagrams from one sender in
/// one buffer (UDP GRO, Linux 5.0): see [`recv_segments`]. `false` where
/// the kernel cannot.
pub fn enable_gro(socket: &UdpSocket) -> bool {
    let on: libc::c_int = 1;
    // SAFETY: setsockopt reads an int from a valid pointer.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_UDP,
            libc::UDP_GRO,
            (&on as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
    }
}

/// Receives into `buf`: one datagram, or with [`enable_gro`] a run of them
/// of one size (the last may be shorter). Returns the bytes and the
/// datagrams' size; iterate with `buf[..len].chunks(size)`. `buf` should
/// hold 64 KiB.
pub fn recv_segments(socket: &UdpSocket, buf: &mut [u8]) -> io::Result<(usize, usize)> {
    #[repr(C, align(8))]
    struct Control([u8; 64]);
    let mut control = Control([0; 64]);
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: msghdr points at our buffer and control space, both alive
    // for the call; the CMSG helpers walk what the kernel wrote.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.0.as_mut_ptr().cast();
        msg.msg_controllen = control.0.len() as _;
        let n = libc::recvmsg(socket.as_raw_fd(), &mut msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let n = n as usize;
        let mut size = n;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_UDP && (*cmsg).cmsg_type == libc::UDP_GRO {
                let gso = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>());
                size = usize::try_from(gso).unwrap_or(n).clamp(1, n.max(1));
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        Ok((n, size))
    }
}

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
    fn one_call_sends_many_datagrams() {
        let a = bind_udp("127.0.0.1:0").unwrap();
        let b = bind_udp("127.0.0.1:0").unwrap();
        b.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        // Ten datagrams of 1200 bytes and a short last one, from two
        // buffers whose boundary is not a datagram's.
        let data: Vec<u8> = (0..12_300u32).map(|i| (i / 1200) as u8).collect();
        let (x, y) = data.split_at(5000);
        let sent = send_segments(
            &a,
            b.local_addr().unwrap(),
            &[IoSlice::new(x), IoSlice::new(y)],
            1200,
        )
        .unwrap();
        assert_eq!(sent, data.len());
        let mut buf = [0u8; 2048];
        for i in 0..11u8 {
            let n = b.recv(&mut buf).unwrap();
            assert_eq!(n, if i < 10 { 1200 } else { 300 });
            assert!(buf[..n].iter().all(|&v| v == i), "datagram {i}");
        }
    }

    #[test]
    fn a_burst_arrives_in_one_call() {
        let a = bind_udp("127.0.0.1:0").unwrap();
        let b = bind_udp("127.0.0.1:0").unwrap();
        b.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let gro = enable_gro(&b);
        let data: Vec<u8> = (0..12_300u32).map(|i| (i / 1200) as u8).collect();
        send_segments(&a, b.local_addr().unwrap(), &[IoSlice::new(&data)], 1200).unwrap();
        let mut buf = vec![0u8; 65_536];
        let mut got = Vec::new();
        let mut calls = 0;
        while got.len() < 11 {
            let (n, size) = recv_segments(&b, &mut buf).unwrap();
            calls += 1;
            got.extend(buf[..n].chunks(size).map(<[u8]>::to_vec));
        }
        for (i, d) in got.iter().enumerate() {
            assert_eq!(d.len(), if i < 10 { 1200 } else { 300 });
            assert!(d.iter().all(|&v| v == i as u8), "datagram {i}");
        }
        if gro {
            assert_eq!(calls, 1, "one receive for the whole burst");
        }
        // A plain datagram still comes alone, with its own size.
        a.send_to(b"hi", b.local_addr().unwrap()).unwrap();
        let (n, size) = recv_segments(&b, &mut buf).unwrap();
        assert_eq!((&buf[..n], size), (&b"hi"[..], 2));
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
