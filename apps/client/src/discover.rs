//! Finding hosts in the LAN: a question to every broadcast address (and to
//! known hosts directly), answers for a moment.
//!
//! The answers are unauthenticated: a host's key is proven only by the
//! handshake when connecting, and pairing proves it with the PIN. A lying
//! answer can at worst add a wrong entry to the list.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use fernsicht_proto::{Discover, MAX_DATAGRAM, Packet};
use fernsicht_secure::PublicKey;

/// A host that answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundHost {
    /// Where its answer came from (its stream port).
    pub addr: SocketAddr,
    pub name: String,
    pub key: PublicKey,
    /// Pairing is open there: a PIN is shown.
    pub pairing: bool,
    /// A client is connected.
    pub busy: bool,
    pub os: String,
    pub gpu: String,
}

/// How often the question goes out during one search (broadcasts over
/// Wi-Fi get lost now and then).
const ROUNDS: u32 = 3;

/// The IPv4 broadcast address of every network this machine is in, with
/// `port`. Falls back to 255.255.255.255 if none is found.
pub fn broadcast_targets(port: u16) -> Vec<SocketAddr> {
    let mut targets = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below with freeifaddrs; the
    // entries are only read while the list lives.
    unsafe {
        if libc::getifaddrs(&mut list) == 0 {
            let mut cur = list;
            while let Some(ifa) = cur.as_ref() {
                let flags = ifa.ifa_flags;
                let wanted = flags & libc::IFF_UP as u32 != 0
                    && flags & libc::IFF_BROADCAST as u32 != 0
                    && flags & libc::IFF_LOOPBACK as u32 == 0;
                let broadcast = ifa.ifa_ifu;
                if wanted
                    && !broadcast.is_null()
                    && i32::from((*broadcast).sa_family) == libc::AF_INET
                {
                    let sin = &*(broadcast as *const libc::sockaddr_in);
                    let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    targets.push(SocketAddr::V4(SocketAddrV4::new(ip, port)));
                }
                cur = ifa.ifa_next;
            }
            libc::freeifaddrs(list);
        }
    }
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        targets.push(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, port)));
    }
    targets
}

/// Asks `targets` which hosts are there and collects the answers for
/// `wait`. A host reachable over several networks is listed once. Sorted
/// by name.
pub fn discover(targets: &[SocketAddr], wait: Duration) -> std::io::Result<Vec<FoundHost>> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(true)?;
    let mut nonce = [0u8; 8];
    fernsicht_secure::random(&mut nonce);
    let nonce = u64::from_le_bytes(nonce);
    let mut question = [0u8; Discover::LEN];
    Discover { nonce }.encode(&mut question);

    let start = Instant::now();
    let mut found: Vec<FoundHost> = Vec::new();
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut rounds = 0;
    loop {
        let elapsed = start.elapsed();
        if elapsed >= wait {
            break;
        }
        if rounds < ROUNDS && elapsed >= wait * rounds / ROUNDS {
            for t in targets {
                // One unreachable network must not stop the others.
                let _ = socket.send_to(&question, t);
            }
            rounds += 1;
        }
        let next_round = wait * rounds / ROUNDS;
        let until = if rounds < ROUNDS { next_round } else { wait };
        let timeout = until
            .saturating_sub(start.elapsed())
            .max(Duration::from_millis(1));
        socket.set_read_timeout(Some(timeout))?;
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            // An ICMP "unreachable" for a direct question to a host that is off.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
            Err(e) => return Err(e),
        };
        let Ok(Packet::Announce(a)) = Packet::decode(&buf[..n]) else {
            continue;
        };
        if a.nonce != nonce || found.iter().any(|f| f.key.0 == a.key) {
            continue;
        }
        found.push(FoundHost {
            addr: from,
            name: a.name.to_owned(),
            key: PublicKey(a.key),
            pairing: a.pairing,
            busy: a.busy,
            os: a.os.to_owned(),
            gpu: a.gpu.to_owned(),
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name).then(a.addr.cmp(&b.addr)));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fernsicht_proto::Announce;

    /// A fake host on localhost answering every question with `name`.
    fn fake_host(name: &'static str, key: u8, answers: usize) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let mut out = [0u8; MAX_DATAGRAM];
            for _ in 0..answers {
                let Ok((n, from)) = socket.recv_from(&mut buf) else {
                    return;
                };
                let Ok(Packet::Discover(d)) = Packet::decode(&buf[..n]) else {
                    continue;
                };
                // A stale answer first: other nonce, must be ignored.
                let stale = Announce {
                    nonce: d.nonce ^ 1,
                    key: [99; 32],
                    pairing: false,
                    busy: false,
                    name: "stale",
                    os: "",
                    gpu: "",
                };
                let n = stale.encode(&mut out);
                socket.send_to(&out[..n], from).unwrap();
                let a = Announce {
                    nonce: d.nonce,
                    key: [key; 32],
                    pairing: true,
                    busy: false,
                    name,
                    os: "Bazzite",
                    gpu: "Radeon · H.264",
                };
                let n = a.encode(&mut out);
                socket.send_to(&out[..n], from).unwrap();
            }
        });
        addr
    }

    #[test]
    fn hosts_answer_and_are_listed_once() {
        let a = fake_host("zentrale", 1, 3);
        let b = fake_host("buero", 2, 3);
        // Nobody listens here: the "unreachable" must not end the search.
        let silent = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let found = discover(&[a, b, silent], Duration::from_millis(300)).unwrap();
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["buero", "zentrale"], "sorted, once each, no stale");
        assert_eq!(found[1].addr, a);
        assert_eq!(found[1].key, PublicKey([1; 32]));
        assert!(found[1].pairing && !found[1].busy);
        assert_eq!(
            (found[1].os.as_str(), found[1].gpu.as_str()),
            ("Bazzite", "Radeon · H.264")
        );
    }

    #[test]
    fn nobody_there_is_an_empty_list() {
        let silent = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let t0 = Instant::now();
        assert!(
            discover(&[silent], Duration::from_millis(100))
                .unwrap()
                .is_empty()
        );
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn broadcast_targets_are_ipv4_broadcasts() {
        let targets = broadcast_targets(47800);
        assert!(!targets.is_empty());
        for t in targets {
            assert_eq!(t.port(), 47800);
            assert!(t.is_ipv4());
            assert!(!t.ip().is_loopback());
        }
    }
}
