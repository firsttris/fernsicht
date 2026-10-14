//! The web viewer over the internet (`docs/internet.md`, “The web
//! viewer”): its page reached at a public name through a tunnel to the
//! host's web server (a Cloudflare Tunnel, say), which carries HTTPS, the
//! PIN, and the offer and answer. The picture then needs a path of its
//! own: the host's public address (STUN) as a candidate, and full ICE, so
//! that the host's probes open its NAT while the browser's come in.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use fernsicht_internet::stun::{self, Class, Message};

use crate::http::Head;

/// The public names the page may be reached at, and the STUN servers.
#[derive(Clone, Debug, Default)]
pub(crate) struct Public {
    /// Exact names (`fernsicht.example.org`) or a wildcard for a domain's
    /// names (`*.trycloudflare.com`).
    pub names: Vec<String>,
    /// STUN servers, `host:port`.
    pub stun: Vec<String>,
}

/// The name in a `Host` header, without the port.
fn name_of(host: &str) -> &str {
    if host.starts_with('[') {
        return host;
    }
    host.rsplit_once(':').map_or(host, |(h, _)| h)
}

/// Whether a request's `Host` is one of the public `names`.
pub(crate) fn is_public(host: Option<&str>, names: &[String]) -> bool {
    let Some(name) = host.map(name_of) else {
        return false;
    };
    names.iter().any(|n| match n.strip_prefix("*.") {
        Some(domain) => {
            name.len() > domain.len() + 1
                && name
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{}", domain.to_ascii_lowercase()))
        }
        None => name.eq_ignore_ascii_case(n),
    })
}

/// The browser's address: for a public request that came through a tunnel
/// on this machine (its connection is from loopback), the one the tunnel
/// names (`CF-Connecting-IP`, else the first of `X-Forwarded-For`); else
/// the connection's. Attempt limits count per this address.
pub(crate) fn browser_address(from: SocketAddr, head: &Head, public: bool) -> SocketAddr {
    if !(public && from.ip().is_loopback()) {
        return from;
    }
    let named = head
        .header("cf-connecting-ip")
        .or_else(|| {
            head.header("x-forwarded-for")
                .and_then(|v| v.split(',').next())
        })
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    named.map_or(from, |ip| SocketAddr::new(ip, 0))
}

/// A browser's way in from the internet: the WebRTC socket, bound to the
/// address the host reaches the internet from, and its public address as
/// a STUN server saw it.
pub(crate) struct PublicPath {
    pub socket: UdpSocket,
    pub reflexive: Option<SocketAddr>,
}

/// How long the STUN servers get.
const GATHER_FOR: Duration = Duration::from_millis(500);

/// Opens the socket for a browser over the internet and asks the STUN
/// servers its public address (blocking up to half a second; on the web
/// server's thread, not the control loop).
pub(crate) fn path(stun_servers: &[String]) -> std::io::Result<PublicPath> {
    let servers: Vec<SocketAddr> = stun::resolve(stun_servers)
        .into_iter()
        .filter(SocketAddr::is_ipv4)
        .collect();
    let toward = servers
        .first()
        .map_or(IpAddr::from([1, 1, 1, 1]), SocketAddr::ip);
    let local = super::offer::local_ip_towards(toward)?;
    let socket = UdpSocket::bind((local, 0))?;
    socket.set_read_timeout(Some(Duration::from_millis(50)))?;
    let mut asked = Vec::new();
    for &server in &servers {
        let mut tx = [0u8; 12];
        fernsicht_secure::random(&mut tx);
        socket.send_to(&stun::binding_request(tx), server)?;
        asked.push((tx, server));
    }
    let until = Instant::now() + GATHER_FOR;
    let mut buf = [0u8; 1500];
    let mut reflexive = None;
    while reflexive.is_none() && !asked.is_empty() && Instant::now() < until {
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            continue;
        };
        if let Ok(m) = Message::parse(&buf[..n])
            && m.class == Class::Success
            && asked
                .iter()
                .any(|(tx, s)| *tx == m.transaction && *s == from)
        {
            reflexive = m.reflexive_address();
        }
    }
    socket.set_read_timeout(None)?;
    Ok(PublicPath { socket, reflexive })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;

    fn head(headers: &[(&str, &str)]) -> Head {
        Head {
            method: Method::Post,
            path: "/api/connect".into(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn public_names_exact_and_wildcard() {
        let names = vec!["fernsicht.example.org".into(), "*.trycloudflare.com".into()];
        for yes in [
            "fernsicht.example.org",
            "Fernsicht.Example.org:443",
            "abc-def.trycloudflare.com",
        ] {
            assert!(is_public(Some(yes), &names), "{yes}");
        }
        for no in [
            "example.org",
            "trycloudflare.com",
            "evil.org",
            "a.trycloudflare.com.evil.org",
        ] {
            assert!(!is_public(Some(no), &names), "{no}");
        }
        assert!(!is_public(None, &names));
        assert!(!is_public(Some("fernsicht.example.org"), &[]));
    }

    #[test]
    fn the_browsers_address_comes_from_a_local_tunnel_only() {
        let tunnel: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let h = head(&[
            ("cf-connecting-ip", "203.0.113.9"),
            ("x-forwarded-for", "198.51.100.1"),
        ]);
        assert_eq!(
            browser_address(tunnel, &h, true).ip().to_string(),
            "203.0.113.9"
        );
        let h = head(&[("x-forwarded-for", "198.51.100.1, 10.0.0.1")]);
        assert_eq!(
            browser_address(tunnel, &h, true).ip().to_string(),
            "198.51.100.1"
        );
        // From elsewhere, or not public, the header is anyone's word.
        let lan: SocketAddr = "192.168.1.9:4000".parse().unwrap();
        assert_eq!(browser_address(lan, &h, true), lan);
        assert_eq!(browser_address(tunnel, &h, false), tunnel);
        let junk = head(&[("x-forwarded-for", "not an ip")]);
        assert_eq!(browser_address(tunnel, &junk, true), tunnel);
    }
}
