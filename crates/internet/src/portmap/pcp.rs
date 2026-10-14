//! PCP (RFC 6887), NAT-PMP's successor: a MAP request asks the router to
//! forward a UDP port. Pure: the request's bytes, the answer's meaning.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

const VERSION: u8 = 2;
const OPCODE_MAP: u8 = 1;
const UDP: u8 = 17;
/// Request header (24 bytes) and MAP's own part (36).
pub const MAP_LEN: usize = 24 + 36;

/// An IP address as PCP writes it: IPv4 mapped into IPv6.
fn wide(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => v6.octets(),
    }
}

/// MAP: forward UDP to `client`'s `internal` port for `lifetime` seconds
/// (0 removes it), `nonce` naming the mapping (the same one renews it).
pub fn map_request(client: IpAddr, internal: u16, lifetime: u32, nonce: [u8; 12]) -> [u8; MAP_LEN] {
    let mut r = [0u8; MAP_LEN];
    r[0] = VERSION;
    r[1] = OPCODE_MAP;
    r[4..8].copy_from_slice(&lifetime.to_be_bytes());
    r[8..24].copy_from_slice(&wide(client));
    r[24..36].copy_from_slice(&nonce);
    r[36] = UDP;
    r[40..42].copy_from_slice(&internal.to_be_bytes());
    // Suggested external port: the same; address: none (zeros, as
    // ::ffff:0.0.0.0 for an IPv4 client).
    r[42..44].copy_from_slice(&internal.to_be_bytes());
    let any = match client {
        IpAddr::V4(_) => Ipv6Addr::from([0, 0, 0, 0, 0, 0xffff, 0, 0]).octets(),
        IpAddr::V6(_) => [0; 16],
    };
    r[44..60].copy_from_slice(&any);
    r
}

/// An answer to a MAP request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Mapped {
        nonce: [u8; 12],
        external: SocketAddr,
        lifetime: u32,
    },
    /// A result code other than success (e.g. 1: unsupported version, as
    /// a NAT-PMP-only router answers; 2: not authorized).
    Refused(u8),
    /// A NAT-PMP router's answer (version 0): try NAT-PMP.
    OnlyNatPmp,
}

pub fn parse(b: &[u8]) -> Option<Answer> {
    let (&version, _) = b.split_first()?;
    if version == 0 {
        return Some(Answer::OnlyNatPmp);
    }
    if version != VERSION || b.len() < 4 || b[1] != (0x80 | OPCODE_MAP) {
        return None;
    }
    if b[3] != 0 {
        return Some(Answer::Refused(b[3]));
    }
    if b.len() < MAP_LEN {
        return None;
    }
    let lifetime = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&b[24..36]);
    let port = u16::from_be_bytes([b[42], b[43]]);
    let mut ip = [0u8; 16];
    ip.copy_from_slice(&b[44..60]);
    let ip = Ipv6Addr::from(ip);
    let ip = ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4);
    Some(Answer::Mapped {
        nonce,
        external: SocketAddr::new(ip, port),
        lifetime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_map_request_has_the_rfcs_layout() {
        let r = map_request("192.168.1.20".parse().unwrap(), 47800, 7200, [7; 12]);
        assert_eq!(&r[..4], &[2, 1, 0, 0]);
        assert_eq!(&r[4..8], &7200u32.to_be_bytes());
        assert_eq!(
            &r[8..24],
            &"::ffff:192.168.1.20".parse::<Ipv6Addr>().unwrap().octets()
        );
        assert_eq!(&r[24..36], &[7; 12]);
        assert_eq!(r[36], 17);
        assert_eq!(&r[40..44], &[0xba, 0xb8, 0xba, 0xb8]);
        assert_eq!(
            &r[44..60],
            &"::ffff:0.0.0.0".parse::<Ipv6Addr>().unwrap().octets()
        );
    }

    #[test]
    fn answers_are_read() {
        let mut a = map_request("192.168.1.20".parse().unwrap(), 47800, 7200, [7; 12]);
        a[1] = 0x81;
        a[44..60].copy_from_slice(&"::ffff:203.0.113.5".parse::<Ipv6Addr>().unwrap().octets());
        a[42..44].copy_from_slice(&47801u16.to_be_bytes());
        assert_eq!(
            parse(&a),
            Some(Answer::Mapped {
                nonce: [7; 12],
                external: "203.0.113.5:47801".parse().unwrap(),
                lifetime: 7200
            })
        );
        a[3] = 2;
        assert_eq!(parse(&a), Some(Answer::Refused(2)));
        assert_eq!(parse(&[0, 128, 0, 1]), Some(Answer::OnlyNatPmp));
        assert_eq!(parse(&[2, 0x81, 0, 0, 1]), None, "too short");
        assert_eq!(parse(&[]), None);
    }
}
