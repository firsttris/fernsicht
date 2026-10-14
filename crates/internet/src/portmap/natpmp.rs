//! NAT-PMP (RFC 6886): asking the router to forward a UDP port. Pure:
//! the requests' bytes and the answers' meaning (libnatpmp's natpmp.c
//! builds the same).

use std::net::Ipv4Addr;

/// The port routers listen on for NAT-PMP and PCP.
pub const SERVER_PORT: u16 = 5351;

/// "What is your public address?" (opcode 0).
pub fn external_address_request() -> [u8; 2] {
    [0, 0]
}

/// "Forward UDP `external` (a wish) to my `internal` port for
/// `lifetime` seconds" (opcode 1); a lifetime of 0 removes it.
pub fn map_request(internal: u16, external: u16, lifetime: u32) -> [u8; 12] {
    let mut r = [0u8; 12];
    r[1] = 1;
    r[4..6].copy_from_slice(&internal.to_be_bytes());
    r[6..8].copy_from_slice(&external.to_be_bytes());
    r[8..12].copy_from_slice(&lifetime.to_be_bytes());
    r
}

/// An answer from the router.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    External(Ipv4Addr),
    Mapped {
        internal: u16,
        external: u16,
        lifetime: u32,
    },
    /// A result code other than success (1: unsupported version, 2: not
    /// authorized, 3: network failure, 4: out of resources, 5:
    /// unsupported opcode).
    Refused(u16),
}

/// Reads an answer; `None` for anything that is not one.
pub fn parse(b: &[u8]) -> Option<Answer> {
    if b.len() < 8 || b[0] != 0 {
        return None;
    }
    let result = u16::from_be_bytes([b[2], b[3]]);
    if result != 0 {
        return Some(Answer::Refused(result));
    }
    match (b[1], b.len()) {
        (128, 12..) => Some(Answer::External(Ipv4Addr::new(b[8], b[9], b[10], b[11]))),
        (129, 16..) => Some(Answer::Mapped {
            internal: u16::from_be_bytes([b[8], b[9]]),
            external: u16::from_be_bytes([b[10], b[11]]),
            lifetime: u32::from_be_bytes([b[12], b[13], b[14], b[15]]),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_as_the_rfc_draws_them() {
        assert_eq!(external_address_request(), [0, 0]);
        assert_eq!(
            map_request(47800, 47800, 7200),
            [0, 1, 0, 0, 0xba, 0xb8, 0xba, 0xb8, 0, 0, 0x1c, 0x20]
        );
    }

    #[test]
    fn answers_are_read() {
        let ext = [0, 128, 0, 0, 0, 0, 0, 9, 203, 0, 113, 5];
        assert_eq!(
            parse(&ext),
            Some(Answer::External(Ipv4Addr::new(203, 0, 113, 5)))
        );
        let map = [
            0, 129, 0, 0, 0, 0, 0, 9, 0xba, 0xb8, 0xba, 0xb9, 0, 0, 0x1c, 0x20,
        ];
        assert_eq!(
            parse(&map),
            Some(Answer::Mapped {
                internal: 47800,
                external: 47801,
                lifetime: 7200
            })
        );
        assert_eq!(parse(&[0, 129, 0, 3, 0, 0, 0, 0]), Some(Answer::Refused(3)));
        assert_eq!(parse(&[2, 129, 0, 0, 0, 0, 0, 0]), None, "PCP, not NAT-PMP");
        assert_eq!(parse(&[0, 129, 0, 0]), None);
        assert_eq!(parse(&map[..12]), None, "too short for a mapping");
    }
}
