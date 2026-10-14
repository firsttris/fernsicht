//! STUN messages (RFC 8489) and TURN's additions to them (RFC 8656):
//! parsing and building, message integrity (HMAC-SHA1) and the
//! fingerprint (CRC-32).
//!
//! Parsing never panics: a malformed message is an [`Error`]. Building
//! allocates (a `Vec`); STUN and TURN control messages are rare (a few per
//! connection), never per frame.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use hmac::{Hmac, Mac};

/// Cloudflare's public STUN server (free, anycast): the default.
pub const DEFAULT_SERVER: &str = "stun.cloudflare.com:3478";

/// The fixed value in every STUN header (RFC 8489, section 5).
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
/// Bytes of the header: type, length, cookie, transaction ID.
pub const HEADER_LEN: usize = 20;

/// A transaction's ID, chosen at random by whoever sends the request.
pub type TransactionId = [u8; 12];

/// The methods Fernsicht uses.
pub mod method {
    pub const BINDING: u16 = 0x001;
    pub const ALLOCATE: u16 = 0x003;
    pub const REFRESH: u16 = 0x004;
    pub const SEND: u16 = 0x006;
    pub const DATA: u16 = 0x007;
    pub const CREATE_PERMISSION: u16 = 0x008;
    pub const CHANNEL_BIND: u16 = 0x009;
}

/// The attributes Fernsicht reads or writes.
pub mod attr {
    pub const MAPPED_ADDRESS: u16 = 0x0001;
    pub const USERNAME: u16 = 0x0006;
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub const ERROR_CODE: u16 = 0x0009;
    pub const CHANNEL_NUMBER: u16 = 0x000C;
    pub const LIFETIME: u16 = 0x000D;
    pub const XOR_PEER_ADDRESS: u16 = 0x0012;
    pub const DATA: u16 = 0x0013;
    pub const REALM: u16 = 0x0014;
    pub const NONCE: u16 = 0x0015;
    pub const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    pub const REQUESTED_TRANSPORT: u16 = 0x0019;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const SOFTWARE: u16 = 0x8022;
    pub const FINGERPRINT: u16 = 0x8028;
}

/// What a message is: a request, an indication (no answer) or a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Request,
    Indication,
    Success,
    Error,
}

impl Class {
    /// The two class bits, C1 C0.
    fn bits(self) -> u16 {
        match self {
            Class::Request => 0b00,
            Class::Indication => 0b01,
            Class::Success => 0b10,
            Class::Error => 0b11,
        }
    }

    fn from_bits(b: u16) -> Self {
        match b & 0b11 {
            0b00 => Class::Request,
            0b01 => Class::Indication,
            0b10 => Class::Success,
            _ => Class::Error,
        }
    }
}

/// The message type field: the method's 12 bits with the class's two bits
/// woven in (RFC 8489, section 5: M11–M7, C1, M6–M4, C0, M3–M0).
fn message_type(method: u16, class: Class) -> u16 {
    let c = class.bits();
    (method & 0x000F)
        | ((method & 0x0070) << 1)
        | ((method & 0x0F80) << 2)
        | ((c & 0b01) << 4)
        | ((c & 0b10) << 7)
}

fn split_type(t: u16) -> (u16, Class) {
    let method = (t & 0x000F) | ((t >> 1) & 0x0070) | ((t >> 2) & 0x0F80);
    let class = ((t >> 4) & 0b01) | ((t >> 7) & 0b10);
    (method, Class::from_bits(class))
}

/// Why bytes are not a STUN message we accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    NotStun,
    BadLength,
    BadAttribute,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::Truncated => "STUN message truncated",
            Error::NotStun => "not a STUN message",
            Error::BadLength => "STUN length does not match",
            Error::BadAttribute => "malformed STUN attribute",
        })
    }
}

impl std::error::Error for Error {}

/// A parsed message; attributes are read from its bytes on demand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message<'a> {
    pub method: u16,
    pub class: Class,
    pub transaction: TransactionId,
    raw: &'a [u8],
}

/// Rounds up to the 4-byte boundary attributes are padded to.
fn padded(n: usize) -> usize {
    n.div_ceil(4) * 4
}

impl<'a> Message<'a> {
    /// The message in `buf`, which must be exactly one message: the
    /// header's length matches, every attribute fits.
    pub fn parse(buf: &'a [u8]) -> Result<Self, Error> {
        let header: &[u8; HEADER_LEN] = buf.first_chunk().ok_or(Error::Truncated)?;
        let t = u16::from_be_bytes([header[0], header[1]]);
        if t & 0xC000 != 0 {
            return Err(Error::NotStun);
        }
        let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
        if u32::from_be_bytes([header[4], header[5], header[6], header[7]]) != MAGIC_COOKIE {
            return Err(Error::NotStun);
        }
        if !len.is_multiple_of(4) || HEADER_LEN + len != buf.len() {
            return Err(Error::BadLength);
        }
        let mut transaction = [0u8; 12];
        transaction.copy_from_slice(&header[8..20]);
        let (method, class) = split_type(t);
        let m = Self {
            method,
            class,
            transaction,
            raw: buf,
        };
        // Every attribute must fit; checked once, so reading them later
        // cannot fail half way.
        let mut rest = &buf[HEADER_LEN..];
        while !rest.is_empty() {
            let Some((head, tail)) = rest.split_first_chunk::<4>() else {
                return Err(Error::BadAttribute);
            };
            let n = usize::from(u16::from_be_bytes([head[2], head[3]]));
            if padded(n) > tail.len() {
                return Err(Error::BadAttribute);
            }
            rest = &tail[padded(n)..];
        }
        Ok(m)
    }

    /// The whole message.
    pub fn bytes(&self) -> &'a [u8] {
        self.raw
    }

    /// Every attribute in order: its type, its offset in the message and
    /// its value (without padding).
    fn attributes_at(&self) -> impl Iterator<Item = (u16, usize, &'a [u8])> + 'a {
        let raw = self.raw;
        let mut pos = HEADER_LEN;
        std::iter::from_fn(move || {
            let head = raw.get(pos..pos + 4)?;
            let t = u16::from_be_bytes([head[0], head[1]]);
            let n = usize::from(u16::from_be_bytes([head[2], head[3]]));
            let value = raw.get(pos + 4..pos + 4 + n)?;
            let at = pos;
            pos += 4 + padded(n);
            Some((t, at, value))
        })
    }

    /// Every attribute in order: its type and value.
    pub fn attributes(&self) -> impl Iterator<Item = (u16, &'a [u8])> + 'a {
        self.attributes_at().map(|(t, _, v)| (t, v))
    }

    /// The first attribute of type `t`, before MESSAGE-INTEGRITY (what
    /// follows it, but the fingerprint, is not covered and is ignored,
    /// RFC 8489 section 14.5).
    pub fn attr(&self, t: u16) -> Option<&'a [u8]> {
        let mut integrity_seen = false;
        self.attributes().find_map(|(at, v)| {
            if integrity_seen && t != attr::FINGERPRINT {
                return None;
            }
            integrity_seen |= at == attr::MESSAGE_INTEGRITY;
            (at == t).then_some(v)
        })
    }

    /// An XOR-…-ADDRESS attribute's address.
    pub fn xor_address(&self, t: u16) -> Option<SocketAddr> {
        decode_xor_address(self.attr(t)?, &self.transaction)
    }

    /// A plain MAPPED-ADDRESS (old servers send only that).
    pub fn mapped_address(&self) -> Option<SocketAddr> {
        decode_address(self.attr(attr::MAPPED_ADDRESS)?)
    }

    /// The address the server saw the request come from.
    pub fn reflexive_address(&self) -> Option<SocketAddr> {
        self.xor_address(attr::XOR_MAPPED_ADDRESS)
            .or_else(|| self.mapped_address())
    }

    /// An error response's code and reason.
    pub fn error_code(&self) -> Option<(u16, &'a str)> {
        let v = self.attr(attr::ERROR_CODE)?;
        let head: &[u8; 4] = v.first_chunk()?;
        let code = u16::from(head[2] & 0x07) * 100 + u16::from(head[3]);
        Some((code, std::str::from_utf8(&v[4..]).unwrap_or("")))
    }

    /// A 32-bit attribute (LIFETIME).
    pub fn u32_attr(&self, t: u16) -> Option<u32> {
        let v: &[u8; 4] = self.attr(t)?.first_chunk()?;
        Some(u32::from_be_bytes(*v))
    }

    /// A text attribute (REALM, NONCE, USERNAME).
    pub fn text(&self, t: u16) -> Option<&'a str> {
        std::str::from_utf8(self.attr(t)?).ok()
    }

    /// Whether MESSAGE-INTEGRITY is there and right for `key` (the
    /// password for short-term credentials, [`long_term_key`] for long-term
    /// ones).
    pub fn check_integrity(&self, key: &[u8]) -> bool {
        let Some((_, at, value)) = self
            .attributes_at()
            .find(|(t, _, _)| *t == attr::MESSAGE_INTEGRITY)
        else {
            return false;
        };
        if value.len() != 20 {
            return false;
        }
        let expected = integrity(&self.raw[..at], at + 24 - HEADER_LEN, key);
        constant_time_eq(&expected, value)
    }

    /// Whether the FINGERPRINT is there, last, and right.
    pub fn check_fingerprint(&self) -> bool {
        let Some((t, at, value)) = self.attributes_at().last() else {
            return false;
        };
        t == attr::FINGERPRINT
            && value.len() == 4
            && fingerprint(&self.raw[..at], at + 8 - HEADER_LEN).to_be_bytes() == value
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// HMAC-SHA1 over `before` (the message up to MESSAGE-INTEGRITY) with
/// the header's length set to `len` (as if the message ended right after
/// that attribute, RFC 8489 section 14.5).
fn integrity(before: &[u8], len: usize, key: &[u8]) -> [u8; 20] {
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(key).expect("HMAC takes any key");
    mac.update(&before[..2]);
    mac.update(&(len as u16).to_be_bytes());
    mac.update(&before[4..]);
    mac.finalize().into_bytes().into()
}

/// CRC-32 (ISO HDLC) over `before` with the length set to `len`, XOR
/// 0x5354554E (RFC 8489, section 14.7).
fn fingerprint(before: &[u8], len: usize) -> u32 {
    let len = (len as u16).to_be_bytes();
    let bytes = before[..2].iter().chain(&len).chain(&before[4..]);
    crc32(bytes) ^ 0x5354_554E
}

fn crc32<'a>(bytes: impl Iterator<Item = &'a u8>) -> u32 {
    !bytes.fold(!0u32, |crc, &b| {
        (0..8).fold(crc ^ u32::from(b), |c, _| {
            if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            }
        })
    })
}

/// The key for long-term credentials: MD5 of `username:realm:password`
/// (RFC 8489, section 9.2.2; the texts as the server knows them, already
/// prepared by OpaqueString).
pub fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    use md5::Digest;
    md5::Md5::digest(format!("{username}:{realm}:{password}").as_bytes()).into()
}

fn decode_address(v: &[u8]) -> Option<SocketAddr> {
    let head: &[u8; 4] = v.first_chunk()?;
    let port = u16::from_be_bytes([head[2], head[3]]);
    let ip = match (head[1], v.len()) {
        (0x01, 8) => IpAddr::V4(Ipv4Addr::new(v[4], v[5], v[6], v[7])),
        (0x02, 20) => {
            let mut o = [0u8; 16];
            o.copy_from_slice(&v[4..20]);
            IpAddr::V6(Ipv6Addr::from(o))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// The mask an XOR address is taken with: the cookie, then the
/// transaction ID (RFC 8489, section 14.2).
fn xor_mask(transaction: &TransactionId) -> [u8; 16] {
    let mut mask = [0u8; 16];
    mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    mask[4..].copy_from_slice(transaction);
    mask
}

fn decode_xor_address(v: &[u8], transaction: &TransactionId) -> Option<SocketAddr> {
    let plain = decode_address(v)?;
    let mask = xor_mask(transaction);
    let port = plain.port() ^ u16::from_be_bytes([mask[0], mask[1]]);
    let ip = match plain.ip() {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            IpAddr::V4(Ipv4Addr::from(std::array::from_fn::<u8, 4, _>(|i| {
                o[i] ^ mask[i]
            })))
        }
        IpAddr::V6(ip) => {
            let o = ip.octets();
            IpAddr::V6(Ipv6Addr::from(std::array::from_fn::<u8, 16, _>(|i| {
                o[i] ^ mask[i]
            })))
        }
    };
    Some(SocketAddr::new(ip, port))
}

fn encode_xor_address(addr: SocketAddr, transaction: &TransactionId) -> Vec<u8> {
    let mask = xor_mask(transaction);
    let port = addr.port() ^ u16::from_be_bytes([mask[0], mask[1]]);
    let mut v = vec![0, 0];
    v.extend_from_slice(&port.to_be_bytes());
    match addr.ip() {
        IpAddr::V4(ip) => {
            v[1] = 0x01;
            v.extend(ip.octets().iter().zip(&mask).map(|(a, m)| a ^ m));
        }
        IpAddr::V6(ip) => {
            v[1] = 0x02;
            v.extend(ip.octets().iter().zip(&mask).map(|(a, m)| a ^ m));
        }
    }
    v
}

/// Builds a message attribute by attribute.
pub struct Builder {
    out: Vec<u8>,
    transaction: TransactionId,
}

impl Builder {
    pub fn new(method: u16, class: Class, transaction: TransactionId) -> Self {
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(&message_type(method, class).to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&transaction);
        Self { out, transaction }
    }

    fn set_len(&mut self) {
        let len = (self.out.len() - HEADER_LEN) as u16;
        self.out[2..4].copy_from_slice(&len.to_be_bytes());
    }

    /// Adds an attribute (padded with zeros to 4 bytes).
    pub fn attr(mut self, t: u16, value: &[u8]) -> Self {
        self.out.extend_from_slice(&t.to_be_bytes());
        self.out
            .extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.out.extend_from_slice(value);
        self.out
            .resize(self.out.len() + padded(value.len()) - value.len(), 0);
        self.set_len();
        self
    }

    pub fn u32_attr(self, t: u16, v: u32) -> Self {
        self.attr(t, &v.to_be_bytes())
    }

    pub fn xor_address(self, t: u16, addr: SocketAddr) -> Self {
        let v = encode_xor_address(addr, &self.transaction);
        self.attr(t, &v)
    }

    /// An ERROR-CODE attribute.
    pub fn error(self, code: u16, reason: &str) -> Self {
        let mut v = vec![0, 0, (code / 100) as u8, (code % 100) as u8];
        v.extend_from_slice(reason.as_bytes());
        self.attr(attr::ERROR_CODE, &v)
    }

    /// MESSAGE-INTEGRITY over everything so far, with `key`.
    pub fn integrity(mut self, key: &[u8]) -> Self {
        let at = self.out.len();
        let mac = integrity(&self.out, at + 24 - HEADER_LEN, key);
        self = self.attr(attr::MESSAGE_INTEGRITY, &mac);
        self
    }

    /// The FINGERPRINT, last.
    pub fn fingerprint(mut self) -> Self {
        let at = self.out.len();
        let crc = fingerprint(&self.out, at + 8 - HEADER_LEN);
        self = self.u32_attr(attr::FINGERPRINT, crc);
        self
    }

    pub fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// A Binding request (RFC 8489): the server answers with the address it
/// saw it come from.
pub fn binding_request(transaction: TransactionId) -> Vec<u8> {
    Builder::new(method::BINDING, Class::Request, transaction)
        .fingerprint()
        .finish()
}

/// The addresses of STUN servers (`host:port`), IPv4 and IPv6, two of
/// each at most per server. Looks them up (DNS): not for a loop that must
/// not wait.
pub fn resolve(servers: &[String]) -> Vec<SocketAddr> {
    use std::net::ToSocketAddrs;
    let mut out = Vec::new();
    for s in servers {
        let Ok(addrs) = s.to_socket_addrs() else {
            log::debug!("STUN server {s}: not found");
            continue;
        };
        let addrs: Vec<SocketAddr> = addrs.collect();
        out.extend(addrs.iter().filter(|a| a.is_ipv4()).take(2));
        out.extend(addrs.iter().filter(|a| a.is_ipv6()).take(2));
    }
    out
}

/// What a datagram on a socket that carries several protocols is, by its
/// first byte (RFC 7983, section 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Demux {
    Stun,
    /// TURN ChannelData: relayed data from a channel.
    ChannelData,
    /// A Fernsicht packet (magic 0xF5).
    Fernsicht,
    Other,
}

pub fn demux(datagram: &[u8]) -> Demux {
    match datagram.first() {
        Some(0..=3) => Demux::Stun,
        Some(64..=79) => Demux::ChannelData,
        Some(&fernsicht_proto::MAGIC) => Demux::Fernsicht,
        _ => Demux::Other,
    }
}

/// The first and last channel number a client may bind (RFC 8656,
/// section 12).
pub const CHANNEL_MIN: u16 = 0x4000;
pub const CHANNEL_MAX: u16 = 0x4FFF;
/// ChannelData's header: channel number and length.
pub const CHANNEL_HEADER_LEN: usize = 4;

/// Writes a ChannelData header for `len` bytes on `channel`.
pub fn channel_header(channel: u16, len: usize) -> [u8; CHANNEL_HEADER_LEN] {
    let c = channel.to_be_bytes();
    let l = (len as u16).to_be_bytes();
    [c[0], c[1], l[0], l[1]]
}

/// The channel and the data of a ChannelData message. Over UDP the data
/// may be followed by padding, which is ignored.
pub fn parse_channel_data(datagram: &[u8]) -> Option<(u16, &[u8])> {
    let (head, rest) = datagram.split_first_chunk::<CHANNEL_HEADER_LEN>()?;
    let channel = u16::from_be_bytes([head[0], head[1]]);
    let len = usize::from(u16::from_be_bytes([head[2], head[3]]));
    if !(CHANNEL_MIN..=CHANNEL_MAX).contains(&channel) {
        return None;
    }
    Some((channel, rest.get(..len)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 5769 test vectors (as coturn's src/apps/rfc5769/rfc5769check.c
    // carries them).
    const SAMPLE_REQUEST: &[u8] = b"\x00\x01\x00\x58\x21\x12\xa4\x42\
        \xb7\xe7\xa7\x01\xbc\x34\xd6\x86\xfa\x87\xdf\xae\
        \x80\x22\x00\x10STUN test client\
        \x00\x24\x00\x04\x6e\x00\x01\xff\
        \x80\x29\x00\x08\x93\x2f\xf9\xb1\x51\x26\x3b\x36\
        \x00\x06\x00\x09\x65\x76\x74\x6a\x3a\x68\x36\x76\x59\x20\x20\x20\
        \x00\x08\x00\x14\x9a\xea\xa7\x0c\xbf\xd8\xcb\x56\x78\x1e\xf2\xb5\
        \xb2\xd3\xf2\x49\xc1\xb5\x71\xa2\
        \x80\x28\x00\x04\xe5\x7a\x3b\xcf";
    const RESPONSE_V4: &[u8] = b"\x01\x01\x00\x3c\x21\x12\xa4\x42\
        \xb7\xe7\xa7\x01\xbc\x34\xd6\x86\xfa\x87\xdf\xae\
        \x80\x22\x00\x0b\x74\x65\x73\x74\x20\x76\x65\x63\x74\x6f\x72\x20\
        \x00\x20\x00\x08\x00\x01\xa1\x47\xe1\x12\xa6\x43\
        \x00\x08\x00\x14\x2b\x91\xf5\x99\xfd\x9e\x90\xc3\x8c\x74\x89\xf9\
        \x2a\xf9\xba\x53\xf0\x6b\xe7\xd7\
        \x80\x28\x00\x04\xc0\x7d\x4c\x96";
    const RESPONSE_V6: &[u8] = b"\x01\x01\x00\x48\x21\x12\xa4\x42\
        \xb7\xe7\xa7\x01\xbc\x34\xd6\x86\xfa\x87\xdf\xae\
        \x80\x22\x00\x0b\x74\x65\x73\x74\x20\x76\x65\x63\x74\x6f\x72\x20\
        \x00\x20\x00\x14\x00\x02\xa1\x47\
        \x01\x13\xa9\xfa\xa5\xd3\xf1\x79\xbc\x25\xf4\xb5\xbe\xd2\xb9\xd9\
        \x00\x08\x00\x14\xa3\x82\x95\x4e\x4b\xe6\x7b\xf1\x17\x84\xc9\x7c\
        \x82\x92\xc2\x75\xbf\xe3\xed\x41\
        \x80\x28\x00\x04\xc8\xfb\x0b\x4c";
    const LONG_TERM_REQUEST: &[u8] = b"\x00\x01\x00\x60\x21\x12\xa4\x42\
        \x78\xad\x34\x33\xc6\xad\x72\xc0\x29\xda\x41\x2e\
        \x00\x06\x00\x12\
        \xe3\x83\x9e\xe3\x83\x88\xe3\x83\xaa\xe3\x83\x83\xe3\x82\xaf\xe3\x82\xb9\x00\x00\
        \x00\x15\x00\x1c\
        \x66\x2f\x2f\x34\x39\x39\x6b\x39\x35\x34\x64\x36\
        \x4f\x4c\x33\x34\x6f\x4c\x39\x46\x53\x54\x76\x79\x36\x34\x73\x41\
        \x00\x14\x00\x0b\x65\x78\x61\x6d\x70\x6c\x65\x2e\x6f\x72\x67\x00\
        \x00\x08\x00\x14\xf6\x70\x24\x65\x6d\xd6\x4a\x3e\x02\xb8\xe0\x71\
        \x2e\x85\xc9\xa2\x8c\xa8\x96\x66";
    const SHORT_TERM_PASSWORD: &[u8] = b"VOkJxbRl1RmTxUk/WvJxBt";

    #[test]
    fn rfc5769_sample_request() {
        let m = Message::parse(SAMPLE_REQUEST).unwrap();
        assert_eq!((m.method, m.class), (method::BINDING, Class::Request));
        assert!(m.check_fingerprint());
        assert!(m.check_integrity(SHORT_TERM_PASSWORD));
        assert!(!m.check_integrity(b"wrong"));
        assert_eq!(m.text(attr::USERNAME), Some("evtj:h6vY"));
        let mut broken = SAMPLE_REQUEST.to_vec();
        broken[27] = 23;
        let m = Message::parse(&broken).unwrap();
        assert!(!m.check_fingerprint());
        assert!(!m.check_integrity(SHORT_TERM_PASSWORD));
    }

    #[test]
    fn rfc5769_responses() {
        for (bytes, want) in [
            (RESPONSE_V4, "192.0.2.1:32853"),
            (RESPONSE_V6, "[2001:db8:1234:5678:11:2233:4455:6677]:32853"),
        ] {
            let m = Message::parse(bytes).unwrap();
            assert_eq!((m.method, m.class), (method::BINDING, Class::Success));
            assert!(m.check_fingerprint() && m.check_integrity(SHORT_TERM_PASSWORD));
            assert_eq!(m.reflexive_address(), Some(want.parse().unwrap()));
        }
    }

    #[test]
    fn rfc5769_long_term_credentials() {
        let m = Message::parse(LONG_TERM_REQUEST).unwrap();
        let user = m.text(attr::USERNAME).unwrap();
        assert_eq!(user, "マトリックス");
        assert_eq!(m.text(attr::REALM), Some("example.org"));
        assert_eq!(m.text(attr::NONCE), Some("f//499k954d6OL34oL9FSTvy64sA"));
        // The password "The<U+00AD>M<U+00AA>tr<U+2168>" after OpaqueString.
        let key = long_term_key(user, "example.org", "TheMatrIX");
        assert!(m.check_integrity(&key));
    }

    #[test]
    fn built_messages_verify_and_carry_addresses() {
        let tx = [7u8; 12];
        let key = long_term_key("u", "r", "p");
        for addr in ["203.0.113.9:47800", "[2001:db8::7]:1"] {
            let addr: SocketAddr = addr.parse().unwrap();
            let bytes = Builder::new(method::ALLOCATE, Class::Success, tx)
                .xor_address(attr::XOR_RELAYED_ADDRESS, addr)
                .u32_attr(attr::LIFETIME, 600)
                .attr(attr::SOFTWARE, b"odd")
                .integrity(&key)
                .fingerprint()
                .finish();
            let m = Message::parse(&bytes).unwrap();
            assert_eq!((m.method, m.class), (method::ALLOCATE, Class::Success));
            assert_eq!(m.transaction, tx);
            assert_eq!(m.xor_address(attr::XOR_RELAYED_ADDRESS), Some(addr));
            assert_eq!(m.u32_attr(attr::LIFETIME), Some(600));
            assert_eq!(m.attr(attr::SOFTWARE), Some(&b"odd"[..]));
            assert!(m.check_integrity(&key) && m.check_fingerprint());
        }
        let err = Builder::new(method::ALLOCATE, Class::Error, tx)
            .error(401, "Unauthorized")
            .finish();
        let m = Message::parse(&err).unwrap();
        assert_eq!(m.class, Class::Error);
        assert_eq!(m.error_code(), Some((401, "Unauthorized")));
        assert!(!m.check_integrity(&key), "no integrity attribute");
        assert!(!m.check_fingerprint(), "no fingerprint");
    }

    #[test]
    fn every_method_and_class_survives_the_type_field() {
        for method in 0..0x1000u16 {
            for class in [
                Class::Request,
                Class::Indication,
                Class::Success,
                Class::Error,
            ] {
                assert_eq!(split_type(message_type(method, class)), (method, class));
            }
        }
    }

    #[test]
    fn attributes_after_integrity_are_ignored() {
        let bytes = Builder::new(method::BINDING, Class::Success, [1; 12])
            .integrity(b"k")
            .attr(attr::SOFTWARE, b"added later")
            .finish();
        let m = Message::parse(&bytes).unwrap();
        assert_eq!(m.attr(attr::SOFTWARE), None);
        assert!(m.check_integrity(b"k"));
    }

    #[test]
    fn malformed_messages_are_errors() {
        let good = binding_request([3; 12]);
        assert!(Message::parse(&good).is_ok());
        assert_eq!(Message::parse(&good[..19]), Err(Error::Truncated));
        let mut cookie = good.clone();
        cookie[4] ^= 1;
        assert_eq!(Message::parse(&cookie), Err(Error::NotStun));
        let mut long = good.clone();
        long.push(0);
        assert_eq!(Message::parse(&long), Err(Error::BadLength));
        let mut attr_len = good.clone();
        attr_len[23] = 200;
        assert_eq!(Message::parse(&attr_len), Err(Error::BadAttribute));
        let mut top_bits = good;
        top_bits[0] |= 0x80;
        assert_eq!(Message::parse(&top_bits), Err(Error::NotStun));
    }

    #[test]
    fn servers_are_looked_up() {
        let found = resolve(&["127.0.0.1:3478".into(), "not a host".into()]);
        assert_eq!(found, ["127.0.0.1:3478".parse::<SocketAddr>().unwrap()]);
    }

    #[test]
    fn datagrams_are_told_apart_by_their_first_byte() {
        assert_eq!(demux(&binding_request([0; 12])), Demux::Stun);
        assert_eq!(demux(&[0x40, 0, 0, 0]), Demux::ChannelData);
        assert_eq!(demux(&[fernsicht_proto::MAGIC, 1]), Demux::Fernsicht);
        assert_eq!(demux(&[0x80]), Demux::Other);
        assert_eq!(demux(&[]), Demux::Other);
    }

    #[test]
    fn channel_data_roundtrips_and_ignores_padding() {
        let mut d = channel_header(0x4001, 3).to_vec();
        d.extend_from_slice(b"abc\0");
        assert_eq!(parse_channel_data(&d), Some((0x4001, &b"abc"[..])));
        assert_eq!(parse_channel_data(&d[..5]), None, "shorter than its length");
        assert_eq!(parse_channel_data(&[0x50, 0, 0, 0]), None, "not a channel");
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..200)) {
            if let Ok(m) = Message::parse(&bytes) {
                let _ = m.attributes().count();
                let _ = (m.reflexive_address(), m.error_code(), m.check_integrity(b"k"),
                         m.check_fingerprint(), m.xor_address(attr::XOR_PEER_ADDRESS));
            }
            let _ = parse_channel_data(&bytes);
        }

        #[test]
        fn random_headers_with_attributes_never_panic(
            t in 0u16..0x4000, tx: [u8; 12],
            attrs in proptest::collection::vec((proptest::prelude::any::<u16>(), proptest::collection::vec(proptest::prelude::any::<u8>(), 0..24)), 0..6)) {
            let mut b = Builder::new(t & 0x0FFF, Class::Request, tx);
            for (at, v) in &attrs {
                b = b.attr(*at, v);
            }
            let bytes = b.finish();
            let m = Message::parse(&bytes).unwrap();
            proptest::prop_assert_eq!(m.attributes().count(), attrs.len());
            let _ = (m.reflexive_address(), m.error_code(), m.xor_address(attr::XOR_RELAYED_ADDRESS));
        }
    }
}
