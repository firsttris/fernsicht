//! UDP sockets with large kernel buffers.
//!
//! A keyframe can be a few hundred kilobytes that arrive within a couple of
//! milliseconds. With the Linux default receive buffer (~208 KiB) the kernel
//! silently drops the tail of such a burst whenever the receiving thread is
//! briefly descheduled (`RcvbufErrors` in `/proc/net/snmp`). The kernel caps
//! the request at `net.core.rmem_max` / `wmem_max`; raise those with
//! `sysctl` if the warning below shows up.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};

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
    // Best effort: an unprivileged process gets at most rmem_max/wmem_max.
    let _ = socket.set_recv_buffer_size(BUFFER_BYTES);
    let _ = socket.set_send_buffer_size(BUFFER_BYTES);
    socket.bind(&addr.into())?;
    // Linux reports twice the usable size (bookkeeping overhead).
    if let Ok(got) = socket.recv_buffer_size()
        && got < BUFFER_BYTES
    {
        log::warn!(
            "UDP receive buffer is {} KiB (wanted {} KiB); raise net.core.rmem_max to avoid drops on keyframes",
            got / 1024,
            BUFFER_BYTES / 1024
        );
    }
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

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
