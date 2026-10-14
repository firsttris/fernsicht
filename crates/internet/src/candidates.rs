//! Candidates: the addresses a device may be reached at, and how good a
//! path to each is (the ladder in `docs/internet.md`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};

/// How a candidate was found (ICE's candidate types, RFC 8445 section 5.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Kind {
    /// An address of one of the device's own interfaces.
    Host,
    /// The public address a STUN server saw (behind a NAT).
    Reflexive,
    /// A port the router forwards (PCP, NAT-PMP, UPnP).
    Mapped,
    /// An address on a TURN server.
    Relayed,
}

/// One address a device may be reached at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Candidate {
    pub kind: Kind,
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub addr: SocketAddr,
}

/// Most candidates a side offers: they all get probed, each costs packets.
pub const MAX_CANDIDATES: usize = 8;

/// How a path is, best first: the ladder's rungs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Rung {
    /// Within a local network (private addresses, a VPN's).
    Lan,
    /// A public address directly: IPv6, or a host with a public IPv4.
    Public,
    /// A port the host's router forwards.
    Forwarded,
    /// Through both NATs, hole punched.
    Punched,
    /// Through a TURN server.
    Relayed,
}

impl Rung {
    /// Whether packets take the internet's own route, not a relay.
    pub fn is_direct(self) -> bool {
        self != Rung::Relayed
    }
}

/// The rung of a path to `remote`; `via_relay` when it is reached through a
/// TURN server.
pub fn rung(remote: &Candidate, via_relay: bool) -> Rung {
    if via_relay || remote.kind == Kind::Relayed {
        return Rung::Relayed;
    }
    match remote.kind {
        Kind::Host if is_local(remote.addr.ip()) => Rung::Lan,
        Kind::Host => Rung::Public,
        Kind::Mapped => Rung::Forwarded,
        Kind::Reflexive | Kind::Relayed => Rung::Punched,
    }
}

/// Whether a relay at `server` can reach `peer`: a relay on the internet
/// reaches only public addresses, one in a local network also local ones.
pub fn relay_reaches(server: SocketAddr, peer: SocketAddr) -> bool {
    !is_local(peer.ip()) || is_local(server.ip())
}

/// Addresses of a local network: private IPv4 (RFC 1918), shared address
/// space (RFC 6598, used by VPNs such as Tailscale and by carrier NAT),
/// link-local, loopback, IPv6 unique local and link-local. Mapped IPv4 in
/// IPv6 counts as its IPv4 address.
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_local_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_local_v4(v4),
            None => {
                v6.is_loopback()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        },
    }
}

fn is_local_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
}

/// Whether `ip` is worth offering as a host candidate: not loopback, not
/// unspecified, not link-local (which needs an interface to make sense),
/// IPv6 only global or unique local.
pub fn worth_offering(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !(v4.is_loopback() || v4.is_unspecified() || v4.is_link_local()),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            (first & 0xe000) == 0x2000 || (first & 0xfe00) == 0xfc00
        }
    }
}

/// The candidates to offer from `found`, best first, without duplicates,
/// at most [`MAX_CANDIDATES`]: a reflexive address that equals a host
/// address (no NAT) is offered once, as host.
pub fn offer_list(found: impl IntoIterator<Item = Candidate>) -> Vec<Candidate> {
    let mut list: Vec<Candidate> = Vec::new();
    for c in found {
        if !list.iter().any(|o| o.addr == c.addr) {
            list.push(c);
        }
    }
    list.sort_by_key(|c| (rung(c, false), c.addr.is_ipv4()));
    list.truncate(MAX_CANDIDATES);
    list
}

/// This machine's own addresses worth offering, each with `port`.
pub fn host_candidates(port: u16) -> Vec<Candidate> {
    interface_addresses()
        .into_iter()
        .filter(|ip| worth_offering(*ip))
        .map(|ip| Candidate {
            kind: Kind::Host,
            addr: SocketAddr::new(ip, port),
        })
        .collect()
}

/// Whether `ip` is in one of `networks` (an address and its netmask).
pub fn in_networks(ip: IpAddr, networks: &[(IpAddr, IpAddr)]) -> bool {
    let ip = ip.to_canonical();
    networks.iter().any(|(addr, mask)| match (ip, addr, mask) {
        (IpAddr::V4(i), IpAddr::V4(a), IpAddr::V4(m)) => {
            u32::from(i) & u32::from(*m) == u32::from(*a) & u32::from(*m)
        }
        (IpAddr::V6(i), IpAddr::V6(a), IpAddr::V6(m)) => {
            u128::from(i) & u128::from(*m) == u128::from(*a) & u128::from(*m)
        }
        _ => false,
    })
}

/// Whether a device at `ip` is a neighbour: in a local network (see
/// [`is_local`]) or in one of this machine's own networks. A host answers
/// discovery only to neighbours, so that a host reachable from the
/// internet does not tell it its name and GPU.
pub fn is_neighbour(ip: IpAddr, own_networks: &[(IpAddr, IpAddr)]) -> bool {
    is_local(ip) || in_networks(ip, own_networks)
}

/// The networks of the interfaces that are up: each address with its
/// netmask (getifaddrs).
pub fn interface_networks() -> Vec<(IpAddr, IpAddr)> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: as in `interface_addresses`; the netmask has the address's
    // family.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while let Some(ifa) = cur.as_ref() {
            let up = ifa.ifa_flags & libc::IFF_UP as u32 != 0;
            if up
                && !ifa.ifa_addr.is_null()
                && !ifa.ifa_netmask.is_null()
                && let (Some(a), Some(m)) =
                    (sockaddr_ip(ifa.ifa_addr), sockaddr_ip(ifa.ifa_netmask))
            {
                out.push((a, m));
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}

/// The IP address in a sockaddr of family INET or INET6.
///
/// # Safety
/// `sa` points to a valid sockaddr of the size its family says.
unsafe fn sockaddr_ip(sa: *const libc::sockaddr) -> Option<IpAddr> {
    // SAFETY: the caller's promise.
    unsafe {
        match i32::from((*sa).sa_family) {
            libc::AF_INET => {
                let sin = &*(sa as *const libc::sockaddr_in);
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    sin.sin_addr.s_addr,
                ))))
            }
            libc::AF_INET6 => {
                let sin6 = &*(sa as *const libc::sockaddr_in6);
                Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }
}

/// The addresses of the interfaces that are up (getifaddrs).
pub fn interface_addresses() -> Vec<IpAddr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list`, freed below with freeifaddrs; the
    // entries are only read while the list lives, each address as the
    // structure its family says it is.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while let Some(ifa) = cur.as_ref() {
            let up = ifa.ifa_flags & libc::IFF_UP as u32 != 0;
            if up && !ifa.ifa_addr.is_null() {
                match i32::from((*ifa.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                        out.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                            sin.sin_addr.s_addr,
                        ))));
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                        out.push(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(kind: Kind, addr: &str) -> Candidate {
        Candidate {
            kind,
            addr: addr.parse().unwrap(),
        }
    }

    #[test]
    fn paths_climb_the_ladder() {
        assert_eq!(
            rung(&c(Kind::Host, "192.168.178.20:47800"), false),
            Rung::Lan
        );
        assert_eq!(
            rung(&c(Kind::Host, "100.101.102.103:47800"), false),
            Rung::Lan
        );
        assert_eq!(
            rung(&c(Kind::Host, "[fd7a:115c::1]:47800"), false),
            Rung::Lan
        );
        assert_eq!(
            rung(&c(Kind::Host, "[2a02:810::5]:47800"), false),
            Rung::Public
        );
        assert_eq!(
            rung(&c(Kind::Host, "203.0.113.5:47800"), false),
            Rung::Public
        );
        assert_eq!(
            rung(&c(Kind::Mapped, "203.0.113.5:47800"), false),
            Rung::Forwarded
        );
        assert_eq!(
            rung(&c(Kind::Reflexive, "203.0.113.5:61000"), false),
            Rung::Punched
        );
        assert_eq!(
            rung(&c(Kind::Relayed, "198.51.100.1:3478"), false),
            Rung::Relayed
        );
        assert_eq!(
            rung(&c(Kind::Host, "192.168.178.20:47800"), true),
            Rung::Relayed
        );
        assert!(Rung::Lan < Rung::Public && Rung::Punched < Rung::Relayed);
        assert!(Rung::Punched.is_direct() && !Rung::Relayed.is_direct());
    }

    #[test]
    fn local_and_offered_addresses() {
        for local in [
            "10.0.0.1",
            "172.16.5.4",
            "192.168.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(is_local(local.parse().unwrap()), "{local}");
        }
        for public in ["8.8.8.8", "100.128.0.1", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!is_local(public.parse().unwrap()), "{public}");
        }
        for yes in ["192.168.1.2", "203.0.113.1", "2a02:810::5", "fd00::1"] {
            assert!(worth_offering(yes.parse().unwrap()), "{yes}");
        }
        for no in [
            "127.0.0.1",
            "0.0.0.0",
            "169.254.3.3",
            "::1",
            "fe80::1",
            "::",
            "ff02::1",
        ] {
            assert!(!worth_offering(no.parse().unwrap()), "{no}");
        }
    }

    #[test]
    fn a_relay_on_the_internet_reaches_only_public_addresses() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let public_relay = a("198.51.100.1:3478");
        assert!(relay_reaches(public_relay, a("203.0.113.5:47800")));
        assert!(!relay_reaches(public_relay, a("192.168.10.2:47800")));
        assert!(!relay_reaches(public_relay, a("[fd00::2]:47800")));
        // One in the LAN (or on this machine, in tests) reaches both.
        assert!(relay_reaches(a("127.0.0.1:3478"), a("127.0.0.1:47800")));
        assert!(relay_reaches(a("192.168.1.1:3478"), a("203.0.113.5:47800")));
    }

    #[test]
    fn the_offer_is_sorted_deduplicated_and_short() {
        let list = offer_list([
            c(Kind::Reflexive, "203.0.113.5:61000"),
            c(Kind::Host, "192.168.1.2:47800"),
            c(Kind::Reflexive, "192.168.1.2:47800"),
            c(Kind::Host, "[2a02:810::5]:47800"),
            c(Kind::Relayed, "198.51.100.1:50000"),
        ]);
        assert_eq!(
            list,
            [
                c(Kind::Host, "192.168.1.2:47800"),
                c(Kind::Host, "[2a02:810::5]:47800"),
                c(Kind::Reflexive, "203.0.113.5:61000"),
                c(Kind::Relayed, "198.51.100.1:50000"),
            ]
        );
        let many = (0..20).map(|i| c(Kind::Host, &format!("10.0.0.{i}:1")));
        assert_eq!(offer_list(many).len(), MAX_CANDIDATES);
    }

    #[test]
    fn neighbours_are_local_or_in_our_networks() {
        let ours = [
            (
                "203.0.113.10".parse().unwrap(),
                "255.255.255.0".parse().unwrap(),
            ),
            (
                "2a02:810::5".parse().unwrap(),
                "ffff:ffff:ffff:ffff::".parse().unwrap(),
            ),
        ];
        for yes in [
            "192.168.1.7",
            "100.64.1.1",
            "203.0.113.99",
            "2a02:810::77",
            "::ffff:203.0.113.4",
        ] {
            assert!(is_neighbour(yes.parse().unwrap(), &ours), "{yes}");
        }
        for no in ["8.8.8.8", "203.0.114.1", "2a02:811::1"] {
            assert!(!is_neighbour(no.parse().unwrap(), &ours), "{no}");
        }
        assert!(!interface_networks().is_empty(), "loopback at least");
    }

    #[test]
    fn this_machine_has_addresses() {
        // At least loopback is up everywhere; none of it is offered.
        assert!(!interface_addresses().is_empty());
        assert!(
            host_candidates(1)
                .iter()
                .all(|c| worth_offering(c.addr.ip()))
        );
    }
}
