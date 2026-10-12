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

use fernsicht_proto::MAX_DATAGRAM;
use socket2::{Domain, Protocol, Socket, Type};

/// Datagrams one [`send_segments`] call may carry (the kernel's
/// `UDP_MAX_SEGMENTS`).
pub const MAX_SEGMENTS: usize = 64;

/// Bytes one [`send_segments`] call may carry: an IP packet holds 65,507,
/// and the chunks the pacer forms grow by sealing.
pub const MAX_SEGMENTS_BYTES: usize = 60_000;

/// How a datagram is marked for the network (DSCP): Wi-Fi (WMM) queues by
/// it, routers may prioritise by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dscp {
    /// AF41: video (WMM video).
    Video = 34,
    /// EF: small and urgent, input, sound, pointer, feedback (WMM voice).
    Interactive = 46,
}

impl Dscp {
    /// The IPv4 TOS / IPv6 traffic class byte (ECN bits clear).
    pub fn tos(self) -> u8 {
        (self as u8) << 2
    }
}

/// ECN codepoints (RFC 3168): the TOS byte's two low bits. ECT(0): the
/// transport reacts to congestion marks; CE: a router marked congestion
/// instead of dropping the packet.
pub const ECT0: u8 = 0b10;
pub const CE: u8 = 0b11;

/// How a datagram is marked: its DSCP, and whether it is ECN-capable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marking {
    pub dscp: Dscp,
    /// ECT(0): only for packets whose congestion marks the receiver
    /// reports and the sender reacts to (the native client's video, D3);
    /// marking others would ask routers for a courtesy nobody returns.
    pub ect: bool,
}

impl Marking {
    /// Video the client reports CE marks for.
    pub const VIDEO_ECN: Marking = Marking {
        dscp: Dscp::Video,
        ect: true,
    };

    pub fn tos(self) -> u8 {
        self.dscp.tos() | if self.ect { ECT0 } else { 0 }
    }
}

impl From<Dscp> for Marking {
    fn from(dscp: Dscp) -> Self {
        Marking { dscp, ect: false }
    }
}

/// Marks everything `socket` sends with `dscp`, unless a send says
/// otherwise.
pub fn set_dscp(socket: &UdpSocket, dscp: Dscp) -> io::Result<()> {
    let tos = libc::c_int::from(dscp.tos());
    let set = |level, option| {
        // SAFETY: setsockopt reads an int from a valid pointer.
        let r = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                level,
                option,
                (&tos as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if r == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    match socket.local_addr()? {
        SocketAddr::V4(_) => set(libc::IPPROTO_IP, libc::IP_TOS),
        SocketAddr::V6(_) => {
            // IPv4 through a v6 socket (mapped addresses) uses the IPv4 one.
            let _ = set(libc::IPPROTO_IP, libc::IP_TOS);
            set(libc::IPPROTO_IPV6, libc::IPV6_TCLASS)
        }
    }
}

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
    mark: Option<Marking>,
) -> io::Result<usize> {
    send_msg(socket, to, bufs, Some(segment), mark.map(Marking::tos))
}

/// Sends one datagram marked `mark`, whatever the socket's marking.
pub fn send_marked(
    socket: &UdpSocket,
    to: SocketAddr,
    buf: &[u8],
    mark: impl Into<Marking>,
) -> io::Result<usize> {
    send_tos(socket, to, buf, mark.into().tos())
}

/// Sends one datagram with the TOS byte `tos` as it stands, ECN bits and
/// all: what a router that marks congestion forwards (the tests' links).
pub fn send_tos(socket: &UdpSocket, to: SocketAddr, buf: &[u8], tos: u8) -> io::Result<usize> {
    send_msg(socket, to, &[IoSlice::new(buf)], None, Some(tos))
}

/// `sendmsg` with a GSO segment size and a marking as control messages.
fn send_msg(
    socket: &UdpSocket,
    to: SocketAddr,
    bufs: &[IoSlice<'_>],
    segment: Option<u16>,
    tos: Option<u8>,
) -> io::Result<usize> {
    #[repr(C, align(8))]
    struct Control([u8; 64]);
    let mut control = Control([0; 64]);
    // The IPv4 option for IPv4 and mapped addresses, else the IPv6 one.
    let tos_level = match to {
        SocketAddr::V6(a) if a.ip().to_ipv4_mapped().is_none() => {
            (libc::IPPROTO_IPV6, libc::IPV6_TCLASS)
        }
        _ => (libc::IPPROTO_IP, libc::IP_TOS),
    };
    // SAFETY: the CMSG helpers on a zeroed, aligned buffer with room for
    // a u16 and an int message (24 + 24 bytes on 64-bit Linux).
    let len = unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_control = control.0.as_mut_ptr().cast();
        msg.msg_controllen = control.0.len() as _;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        let mut used = 0;
        if let Some(segment) = segment {
            (*cmsg).cmsg_level = libc::SOL_UDP;
            (*cmsg).cmsg_type = libc::UDP_SEGMENT;
            (*cmsg).cmsg_len = libc::CMSG_LEN(2) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<u16>(), segment);
            used += libc::CMSG_SPACE(2) as usize;
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        if let Some(tos) = tos {
            let size = std::mem::size_of::<libc::c_int>() as u32;
            (*cmsg).cmsg_level = tos_level.0;
            (*cmsg).cmsg_type = tos_level.1;
            (*cmsg).cmsg_len = libc::CMSG_LEN(size) as _;
            let tos = libc::c_int::from(tos);
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), tos);
            used += libc::CMSG_SPACE(size) as usize;
        }
        used
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

/// Lets [`recv_segments`] report the datagrams' ECN bits (IP_RECVTOS,
/// IPV6_RECVTCLASS). `false` where the kernel refuses.
pub fn enable_ecn_reports(socket: &UdpSocket) -> bool {
    let on: libc::c_int = 1;
    let set = |level, option| {
        // SAFETY: setsockopt reads an int from a valid pointer.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                level,
                option,
                (&on as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) == 0
        }
    };
    match socket.local_addr() {
        Ok(SocketAddr::V4(_)) => set(libc::IPPROTO_IP, libc::IP_RECVTOS),
        // IPv4 through a v6 socket (mapped addresses) reports IP_TOS.
        Ok(SocketAddr::V6(_)) => {
            let v4 = set(libc::IPPROTO_IP, libc::IP_RECVTOS);
            set(libc::IPPROTO_IPV6, libc::IPV6_RECVTCLASS) || v4
        }
        Err(_) => false,
    }
}

/// What one [`recv_segments`] call brought.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    /// Bytes in the buffer.
    pub len: usize,
    /// The datagrams' size: iterate with `buf[..len].chunks(size)`.
    pub size: usize,
    /// They arrived marked CE (with [`enable_ecn_reports`]). The kernel
    /// coalesces only datagrams of one marking, so it holds for all.
    pub ce: bool,
}

impl Received {
    /// How many datagrams it holds.
    pub fn datagrams(&self) -> usize {
        self.len.div_ceil(self.size.max(1))
    }
}

/// Receives into `buf`: one datagram, or with [`enable_gro`] a run of them
/// of one size (the last may be shorter). `buf` should hold 64 KiB.
pub fn recv_segments(socket: &UdpSocket, buf: &mut [u8]) -> io::Result<Received> {
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
        let mut ce = false;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            let data = libc::CMSG_DATA(cmsg);
            match ((*cmsg).cmsg_level, (*cmsg).cmsg_type) {
                (libc::SOL_UDP, libc::UDP_GRO) => {
                    let gso = std::ptr::read_unaligned(data.cast::<libc::c_int>());
                    size = usize::try_from(gso).unwrap_or(n).clamp(1, n.max(1));
                }
                // IPv4 hands the TOS over as one byte, IPv6 its traffic
                // class as an int (ip(7), ipv6(7)).
                (libc::IPPROTO_IP, libc::IP_TOS) => ce = *data & 0b11 == CE,
                (libc::IPPROTO_IPV6, libc::IPV6_TCLASS) => {
                    let class = std::ptr::read_unaligned(data.cast::<libc::c_int>());
                    ce = class & 0b11 == libc::c_int::from(CE);
                }
                _ => {}
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        Ok(Received { len: n, size, ce })
    }
}

/// The path MTU towards `peer` as the routing table knows it, e.g. a
/// WireGuard interface's 1420 or Tailscale's 1280: a socket connected to
/// it is asked; nothing is sent.
pub fn path_mtu(peer: SocketAddr) -> Option<usize> {
    let socket = UdpSocket::bind(local_for(&peer)).ok()?;
    socket.connect(peer).ok()?;
    let (level, option) = match peer {
        SocketAddr::V6(a) if a.ip().to_ipv4_mapped().is_none() => {
            (libc::IPPROTO_IPV6, libc::IPV6_MTU)
        }
        _ => (libc::IPPROTO_IP, libc::IP_MTU),
    };
    let mut mtu: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt writes an int into a valid pointer.
    let r = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            level,
            option,
            (&mut mtu as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    (r == 0 && mtu > 0).then_some(mtu as usize)
}

/// The UDP payload a path of `mtu` bytes carries without fragments (IPv4
/// or IPv6 and UDP headers off), at most `MAX_DATAGRAM`; that much when
/// the MTU is not known.
pub fn datagram_for(mtu: Option<usize>, v6: bool) -> usize {
    const SMALLEST: usize = 512;
    let headers = if v6 { 40 + 8 } else { 20 + 8 };
    mtu.map_or(MAX_DATAGRAM, |m| m.saturating_sub(headers))
        .clamp(SMALLEST, MAX_DATAGRAM)
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
            None,
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
        send_segments(
            &a,
            b.local_addr().unwrap(),
            &[IoSlice::new(&data)],
            1200,
            None,
        )
        .unwrap();
        let mut buf = vec![0u8; 65_536];
        let mut got = Vec::new();
        let mut calls = 0;
        while got.len() < 11 {
            let Received { len: n, size, .. } = recv_segments(&b, &mut buf).unwrap();
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
        let Received { len: n, size, .. } = recv_segments(&b, &mut buf).unwrap();
        assert_eq!((&buf[..n], size), (&b"hi"[..], 2));
    }

    /// The TOS byte of the next datagram on `b` (IP_RECVTOS on).
    fn received_tos(b: &UdpSocket) -> u8 {
        #[repr(C, align(8))]
        struct Control([u8; 64]);
        let mut control = Control([0; 64]);
        let mut data = [0u8; 2048];
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr().cast(),
            iov_len: data.len(),
        };
        // SAFETY: as in recv_segments.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.0.as_mut_ptr().cast();
            msg.msg_controllen = control.0.len() as _;
            assert!(libc::recvmsg(b.as_raw_fd(), &mut msg, 0) > 0);
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            assert!(!cmsg.is_null(), "no TOS reported");
            assert_eq!(
                ((*cmsg).cmsg_level, (*cmsg).cmsg_type),
                (libc::IPPROTO_IP, libc::IP_TOS)
            );
            *libc::CMSG_DATA(cmsg)
        }
    }

    #[test]
    fn datagrams_carry_their_marking() {
        let a = bind_udp("127.0.0.1:0").unwrap();
        let b = bind_udp("127.0.0.1:0").unwrap();
        b.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let on: libc::c_int = 1;
        // SAFETY: setsockopt reads an int.
        let r = unsafe {
            libc::setsockopt(
                b.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_RECVTOS,
                (&on as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(r, 0);
        let to = b.local_addr().unwrap();
        set_dscp(&a, Dscp::Interactive).unwrap();
        a.send_to(b"input", to).unwrap();
        assert_eq!(received_tos(&b), 46 << 2, "the socket's marking (EF)");
        send_marked(&a, to, b"frame", Dscp::Video).unwrap();
        assert_eq!(received_tos(&b), 34 << 2, "the send's own (AF41)");
        send_segments(
            &a,
            to,
            &[IoSlice::new(&[1u8; 2400])],
            1200,
            Some(Marking::VIDEO_ECN),
        )
        .unwrap();
        assert_eq!(received_tos(&b), 34 << 2 | ECT0, "ECN-capable");
        assert_eq!(received_tos(&b), 34 << 2 | ECT0, "each GSO datagram");
    }

    #[test]
    fn a_congestion_mark_is_reported() {
        let a = bind_udp("127.0.0.1:0").unwrap();
        let b = bind_udp("127.0.0.1:0").unwrap();
        b.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        assert!(enable_ecn_reports(&b));
        let to = b.local_addr().unwrap();
        let mut buf = vec![0u8; 65_536];
        send_marked(&a, to, b"plain", Marking::VIDEO_ECN).unwrap();
        let got = recv_segments(&b, &mut buf).unwrap();
        assert!(!got.ce);
        // What a router on the way would do: CE instead of a drop.
        let ce = Marking::VIDEO_ECN.tos() | CE;
        send_tos(&a, to, b"marked", ce).unwrap();
        let got = recv_segments(&b, &mut buf).unwrap();
        assert_eq!((got.len, got.datagrams(), got.ce), (6, 1, true));
    }

    #[test]
    fn datagrams_fit_the_path() {
        // No MTU known, or a LAN's 1500: the protocol's own limit.
        assert_eq!(datagram_for(None, false), MAX_DATAGRAM);
        assert_eq!(datagram_for(Some(1500), false), MAX_DATAGRAM);
        // WireGuard (1420) and Tailscale (1280), over IPv4 and IPv6.
        assert_eq!(datagram_for(Some(1420), false), 1392);
        assert_eq!(datagram_for(Some(1420), true), 1372);
        assert_eq!(datagram_for(Some(1280), false), 1252);
        assert_eq!(datagram_for(Some(1280), true), 1232);
        // Nonsense stays usable.
        assert_eq!(datagram_for(Some(100), false), 512);
        // The loopback interface reports its own (large) MTU.
        let lo = path_mtu("127.0.0.1:9".parse().unwrap()).unwrap();
        assert!(lo >= 1500, "{lo}");
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
