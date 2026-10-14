//! Port forwarding against routers played on this machine: one that
//! speaks PCP, one that speaks only NAT-PMP (answers PCP with version 0),
//! one that speaks only UPnP IGD (SSDP and SOAP over HTTP), one that
//! refuses. Each is asked as the host would ask its router.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fernsicht_internet::portmap::{self, Method, Router, natpmp, pcp, upnp};

const PUBLIC: [u8; 4] = [203, 0, 113, 5];

/// A UDP router that answers what `answer` makes of each request, and
/// keeps the requests.
fn udp_router(answer: fn(&[u8]) -> Vec<Vec<u8>>) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let keep = seen.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1100];
        while let Ok((n, from)) = socket.recv_from(&mut buf) {
            keep.lock().unwrap().push(buf[..n].to_vec());
            for a in answer(&buf[..n]) {
                let _ = socket.send_to(&a, from);
            }
        }
    });
    (addr, seen)
}

fn router(gateway: SocketAddr, ssdp: SocketAddr) -> Router {
    Router { gateway, ssdp }
}

/// Nobody listens there (SSDP for routers that do not speak UPnP).
fn nobody() -> SocketAddr {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// A PCP router: the mapping on the port asked for, at the public address.
fn speaks_pcp(req: &[u8]) -> Vec<Vec<u8>> {
    if req.first() != Some(&2) {
        return vec![];
    }
    let mut a = req.to_vec();
    a[1] |= 0x80;
    a[8..12].copy_from_slice(&5u32.to_be_bytes()); // epoch
    a[44..60].copy_from_slice(&std::net::Ipv4Addr::from(PUBLIC).to_ipv6_mapped().octets());
    vec![a]
}

/// A NAT-PMP-only router: PCP gets "unsupported version" in NAT-PMP's
/// form.
fn speaks_natpmp(req: &[u8]) -> Vec<Vec<u8>> {
    match req {
        [2, ..] => vec![vec![0, 0x80 | req[1], 0, 1, 0, 0, 0, 0]],
        [0, 0] => {
            let mut a = vec![0, 128, 0, 0, 0, 0, 0, 7];
            a.extend_from_slice(&PUBLIC);
            vec![a]
        }
        [0, 1, _, _, ip0, ip1, _, _, l0, l1, l2, l3] => {
            vec![vec![
                0, 129, 0, 0, 0, 0, 0, 7, *ip0, *ip1, *ip0, *ip1, *l0, *l1, *l2, *l3,
            ]]
        }
        _ => vec![],
    }
}

fn refuses(req: &[u8]) -> Vec<Vec<u8>> {
    let mut a = req.to_vec();
    if a.len() < 4 {
        return vec![];
    }
    a[1] |= 0x80;
    a[3] = 2; // not authorized
    vec![a]
}

#[test]
fn a_pcp_router_forwards_and_renews_with_the_same_nonce() {
    let (gw, seen) = udp_router(speaks_pcp);
    let r = router(gw, nobody());
    let m = portmap::map(&r, 47800, 7200, None).unwrap();
    assert_eq!(m.external, SocketAddr::from((PUBLIC, 47800)));
    assert_eq!(m.lifetime, 7200);
    assert_eq!(m.renew_after(), Duration::from_secs(3600));
    let Method::Pcp { nonce } = m.method else {
        panic!("{m:?}")
    };
    let again = portmap::map(&r, 47800, 7200, Some(&m)).unwrap();
    assert_eq!(
        again.method,
        Method::Pcp { nonce },
        "renewed, not a new one"
    );
    portmap::unmap(&r, &m);
    let requests = seen.lock().unwrap();
    let last = requests.last().unwrap();
    assert_eq!(&last[4..8], &[0, 0, 0, 0], "removal: lifetime 0");
    assert_eq!(&last[24..36], &nonce);
    let _ = pcp::MAP_LEN;
}

#[test]
fn a_natpmp_router_is_asked_after_pcp() {
    let (gw, seen) = udp_router(speaks_natpmp);
    let r = router(gw, nobody());
    let m = portmap::map(&r, 47800, 3600, None).unwrap();
    assert_eq!(m.method, Method::NatPmp);
    assert_eq!(m.external, SocketAddr::from((PUBLIC, 47800)));
    portmap::unmap(&r, &m);
    let requests = seen.lock().unwrap();
    assert_eq!(requests[0][0], 2, "PCP first");
    assert_eq!(
        requests.last().unwrap(),
        &natpmp::map_request(47800, 0, 0).to_vec()
    );
}

#[test]
fn a_refusal_is_the_answer() {
    let (gw, _) = udp_router(refuses);
    let e = portmap::map(&router(gw, nobody()), 47800, 3600, None).unwrap_err();
    assert!(e.contains("refused"), "{e}");
}

#[test]
fn a_router_without_any_says_so() {
    let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
    let r = router(silent.local_addr().unwrap(), nobody());
    let e = portmap::map(&r, 47800, 3600, None).unwrap_err();
    assert!(e.contains("no PCP, NAT-PMP or UPnP"), "{e}");
}

/// An HTTP server answering each request with what `answer` makes of it.
fn http_server(
    answer: impl Fn(&str) -> String + Send + 'static,
) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let keep = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            s.set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            // Headers, then as much body as Content-Length says.
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&req);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let want = head
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|n| n.parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= want {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&req).into_owned();
            let reply = answer(&text);
            keep.lock().unwrap().push(text);
            let _ = s.write_all(reply.as_bytes());
        }
    });
    (addr, seen)
}

#[test]
fn a_upnp_router_is_found_by_ssdp_and_forwards() {
    let (http, seen) = http_server(|req| {
        let ok = |body: &str| format!("HTTP/1.0 200 OK\r\nContent-Type: text/xml\r\n\r\n{body}");
        if req.starts_with("GET /igddesc.xml") {
            ok("<root><device><serviceList><service>\
                <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>\
                <controlURL>/igdupnp/control/WANIPConn1</controlURL></service></serviceList></device></root>")
        } else if req.contains("#AddPortMapping") && req.contains("<NewLeaseDuration>3600<") {
            // Only permanent leases (as some routers): 725.
            "HTTP/1.0 500 Internal Server Error\r\n\r\n<s:Envelope><s:Body><s:Fault><detail><UPnPError>\
             <errorCode>725</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
                .into()
        } else if req.contains("#AddPortMapping") || req.contains("#DeletePortMapping") {
            ok("<s:Envelope><s:Body/></s:Envelope>")
        } else if req.contains("#GetExternalIPAddress") {
            ok("<s:Envelope><s:Body><u:GetExternalIPAddressResponse>\
                <NewExternalIPAddress>203.0.113.5</NewExternalIPAddress>\
                </u:GetExternalIPAddressResponse></s:Body></s:Envelope>")
        } else {
            "HTTP/1.0 404 Not Found\r\n\r\n".into()
        }
    });
    // SSDP's answer points at the description.
    let ssdp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let ssdp_addr = ssdp.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok((n, from)) = ssdp.recv_from(&mut buf) {
            if String::from_utf8_lossy(&buf[..n]).starts_with("M-SEARCH") {
                let a = format!("HTTP/1.1 200 OK\r\nLOCATION: http://{http}/igddesc.xml\r\n\r\n");
                let _ = ssdp.send_to(a.as_bytes(), from);
            }
        }
    });
    let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
    let r = router(silent.local_addr().unwrap(), ssdp_addr);
    let m = portmap::map(&r, 47800, 3600, None).unwrap();
    assert_eq!(m.external, SocketAddr::from((PUBLIC, 47800)));
    assert_eq!(m.lifetime, 0, "a permanent lease after 725");
    assert!(
        matches!(&m.method, Method::Upnp { control, .. } if control.ends_with("/igdupnp/control/WANIPConn1"))
    );
    portmap::unmap(&r, &m);
    let requests = seen.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|r| r.contains("<NewInternalClient>127.0.0.1</NewInternalClient>"))
    );
    assert!(requests.last().unwrap().contains("#DeletePortMapping"));
    let _ = upnp::SSDP;
}
