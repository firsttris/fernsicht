//! Asking the router to forward the host's port (`docs/internet.md`,
//! “Port forwarding by the router”): PCP first, then NAT-PMP, then UPnP
//! IGD. The protocols' bytes are in [`pcp`], [`natpmp`] and [`upnp`]
//! (pure); this module talks to the router (the edge: UDP, a little
//! HTTP, timeouts).

pub mod gateway;
pub mod natpmp;
pub mod pcp;
pub mod upnp;

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

/// Where the router answers. Fixed in use (the gateway's port 5351, SSDP's
/// multicast address); other addresses in tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Router {
    /// PCP and NAT-PMP.
    pub gateway: SocketAddr,
    /// Where SSDP questions go.
    pub ssdp: SocketAddr,
}

impl Router {
    /// This machine's router, if it has a default route.
    pub fn find() -> Option<Self> {
        let g = gateway::find()?;
        Some(Self {
            gateway: SocketAddr::new(IpAddr::V4(g), natpmp::SERVER_PORT),
            ssdp: upnp::SSDP.parse().expect("a valid address"),
        })
    }
}

/// How a mapping was made, to renew or remove it the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    Pcp { nonce: [u8; 12] },
    NatPmp,
    Upnp { service: String, control: String },
}

/// A port the router forwards to the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    /// The public address and port to reach the host at.
    pub external: SocketAddr,
    /// Seconds it lasts (0: until removed); renewed at half.
    pub lifetime: u32,
    pub method: Method,
    /// The host's address in the LAN and its port.
    pub internal: SocketAddr,
}

impl Mapping {
    /// When to renew it, counted from when it was made.
    pub fn renew_after(&self) -> Duration {
        match self.lifetime {
            0 => Duration::from_secs(3600),
            l => Duration::from_secs(u64::from(l) / 2),
        }
    }
}

/// How long one UDP question waits, doubling (as libnatpmp does from 250
/// ms) over three tries.
const FIRST_WAIT: Duration = Duration::from_millis(250);
const TRIES: u32 = 3;

/// Sends `request` to `to` and waits for an answer `read` accepts.
fn ask<T>(to: SocketAddr, request: &[u8], read: impl Fn(&[u8]) -> Option<T>) -> Option<T> {
    let any = if to.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let socket = UdpSocket::bind(any).ok()?;
    let mut buf = [0u8; 1100];
    for attempt in 0..TRIES {
        socket.send_to(request, to).ok()?;
        let until = Instant::now() + FIRST_WAIT * (1 << attempt);
        while Instant::now() < until {
            socket
                .set_read_timeout(Some(
                    until
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(1)),
                ))
                .ok()?;
            match socket.recv_from(&mut buf) {
                Ok((n, from)) if from.ip() == to.ip() => {
                    if let Some(answer) = read(&buf[..n]) {
                        return Some(answer);
                    }
                }
                Ok(_) => {}
                // An ICMP "port unreachable": nobody speaks it there.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return None,
                Err(_) => break,
            }
        }
    }
    None
}

/// The host's address towards the router (the one the router forwards
/// to): a UDP socket "connected" to it says which it would send from.
pub fn local_address_toward(router: SocketAddr) -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(router).ok()?;
    Some(s.local_addr().ok()?.ip())
}

fn with_pcp(
    router: &Router,
    internal: SocketAddr,
    lifetime: u32,
    nonce: [u8; 12],
) -> Result<Mapping, Option<String>> {
    let request = pcp::map_request(internal.ip(), internal.port(), lifetime, nonce);
    match ask(router.gateway, &request, pcp::parse) {
        Some(pcp::Answer::Mapped {
            nonce: n,
            external,
            lifetime,
        }) if n == nonce => Ok(Mapping {
            external,
            lifetime,
            method: Method::Pcp { nonce },
            internal,
        }),
        Some(pcp::Answer::Refused(code)) => Err(Some(format!("PCP: refused ({code})"))),
        // A NAT-PMP router, a wrong nonce, or no answer: the next way.
        _ => Err(None),
    }
}

fn with_natpmp(
    router: &Router,
    internal: SocketAddr,
    lifetime: u32,
) -> Result<Mapping, Option<String>> {
    let port = internal.port();
    let mapped = ask(
        router.gateway,
        &natpmp::map_request(port, port, lifetime),
        |b| match natpmp::parse(b)? {
            a @ (natpmp::Answer::Mapped { .. } | natpmp::Answer::Refused(_)) => Some(a),
            natpmp::Answer::External(_) => None,
        },
    );
    let (external_port, lifetime) = match mapped {
        Some(natpmp::Answer::Mapped {
            external, lifetime, ..
        }) => (external, lifetime),
        Some(natpmp::Answer::Refused(code)) => {
            return Err(Some(format!("NAT-PMP: refused ({code})")));
        }
        _ => return Err(None),
    };
    let ip = ask(
        router.gateway,
        &natpmp::external_address_request(),
        |b| match natpmp::parse(b)? {
            natpmp::Answer::External(ip) => Some(ip),
            _ => None,
        },
    )
    .ok_or_else(|| Some("NAT-PMP: no public address".to_owned()))?;
    Ok(Mapping {
        external: SocketAddr::new(IpAddr::V4(ip), external_port),
        lifetime,
        method: Method::NatPmp,
        internal,
    })
}

/// Plain HTTP in the LAN: sends `request`, reads the whole answer.
fn http(addr: &str, request: &str) -> Option<String> {
    let addr: SocketAddr = std::net::ToSocketAddrs::to_socket_addrs(addr)
        .ok()?
        .next()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(3))).ok()?;
    s.write_all(request.as_bytes()).ok()?;
    let mut out = Vec::new();
    s.take(256 * 1024).read_to_end(&mut out).ok()?;
    String::from_utf8(out).ok()
}

/// `host:port` of an `http://` URL.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("http://")?;
    Some(rest.split('/').next().unwrap_or(rest))
}

fn http_get(url: &str) -> Option<String> {
    let host = host_of(url)?;
    let path = &url["http://".len() + host.len()..];
    let path = if path.is_empty() { "/" } else { path };
    let answer = http(
        host,
        &format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )?;
    let (_, body) = answer.split_once("\r\n\r\n")?;
    Some(body.to_owned())
}

/// The router's port-forwarding service, by SSDP.
fn upnp_service(router: &Router) -> Option<(String, String)> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok()?;
    let question = upnp::search_request();
    let until = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 2048];
    let mut sent = 0;
    while Instant::now() < until {
        if sent < 2 {
            socket.send_to(question.as_bytes(), router.ssdp).ok()?;
            sent += 1;
        }
        let Ok((n, _)) = socket.recv_from(&mut buf) else {
            continue;
        };
        let answer = String::from_utf8_lossy(&buf[..n]);
        if let Some(loc) = upnp::location(&answer)
            && let Some(desc) = http_get(loc)
            && let Some(found) = upnp::control(&desc, loc)
        {
            return Some(found);
        }
    }
    None
}

fn soap(control: &str, request: &str) -> Option<String> {
    http(host_of(control)?, request)
}

fn with_upnp(
    router: &Router,
    internal: SocketAddr,
    lifetime: u32,
    known: Option<(String, String)>,
) -> Result<Mapping, Option<String>> {
    let Some((service, control)) = known.or_else(|| upnp_service(router)) else {
        return Err(None);
    };
    let client = internal.ip().to_string();
    let add = |lifetime: u32| {
        let req = upnp::add_port_mapping(&control, &service, &client, internal.port(), lifetime)?;
        soap(&control, &req)
    };
    let mut lifetime = lifetime;
    let mut answer =
        add(lifetime).ok_or_else(|| Some("UPnP: the router did not answer".to_owned()))?;
    // 725: only permanent leases; those are removed when the host stops.
    if upnp::reply(&answer) == upnp::Reply::Fault(725) {
        lifetime = 0;
        answer = add(0).ok_or_else(|| Some("UPnP: the router did not answer".to_owned()))?;
    }
    match upnp::reply(&answer) {
        upnp::Reply::Ok(_) => {}
        upnp::Reply::Fault(code) => return Err(Some(format!("UPnP: refused ({code})"))),
        upnp::Reply::Malformed => return Err(Some("UPnP: an odd answer".to_owned())),
    }
    let ip = upnp::get_external_ip(&control, &service)
        .and_then(|req| soap(&control, &req))
        .and_then(|a| match upnp::reply(&a) {
            upnp::Reply::Ok(body) => upnp::external_ip(body),
            _ => None,
        })
        .ok_or_else(|| Some("UPnP: no public address".to_owned()))?;
    Ok(Mapping {
        external: SocketAddr::new(ip, internal.port()),
        lifetime,
        method: Method::Upnp { service, control },
        internal,
    })
}

/// Asks the router to forward UDP `port` to this machine for `lifetime`
/// seconds; renews `previous` the way it was made. Why not, if no way
/// worked.
pub fn map(
    router: &Router,
    port: u16,
    lifetime: u32,
    previous: Option<&Mapping>,
) -> Result<Mapping, String> {
    let client = local_address_toward(router.gateway).ok_or("no route to the router")?;
    let internal = SocketAddr::new(client, port);
    let tried = match previous.map(|m| &m.method) {
        Some(Method::Pcp { nonce }) => with_pcp(router, internal, lifetime, *nonce),
        Some(Method::NatPmp) => with_natpmp(router, internal, lifetime),
        Some(Method::Upnp { service, control }) => with_upnp(
            router,
            internal,
            lifetime,
            Some((service.clone(), control.clone())),
        ),
        None => {
            let mut nonce = [0u8; 12];
            fernsicht_secure::random(&mut nonce);
            // Each way's refusal is the answer; only silence tries the next.
            match with_pcp(router, internal, lifetime, nonce) {
                Err(None) => match with_natpmp(router, internal, lifetime) {
                    Err(None) => with_upnp(router, internal, lifetime, None),
                    natpmp => natpmp,
                },
                pcp => pcp,
            }
        }
    };
    tried.map_err(|e| {
        e.unwrap_or_else(|| "the router forwards no ports (no PCP, NAT-PMP or UPnP)".into())
    })
}

/// Asks the router to stop forwarding.
pub fn unmap(router: &Router, m: &Mapping) {
    let _ = match &m.method {
        Method::Pcp { nonce } => with_pcp(router, m.internal, 0, *nonce).map(drop),
        Method::NatPmp => {
            let port = m.internal.port();
            ask(
                router.gateway,
                &natpmp::map_request(port, 0, 0),
                natpmp::parse,
            );
            Ok(())
        }
        Method::Upnp { service, control } => {
            if let Some(req) = upnp::delete_port_mapping(control, service, m.internal.port()) {
                soap(control, &req);
            }
            Ok(())
        }
    };
}
