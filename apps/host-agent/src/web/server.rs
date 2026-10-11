//! The web server: the viewer page, `/api/info` and `/api/connect`, and
//! the checks that keep other sites and names out.

use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Sender, bounded};
use serde::{Deserialize, Serialize};
use str0m::change::{SdpAnswer, SdpOffer};

use super::offer::local_ip_towards;
use super::{WebError, WebRequest};
use crate::HostSecurity;
use crate::http::{self, Body, Head, Method, Response};

/// Serves the viewer page from `root` and the API until `stop`.
pub(crate) fn serve(
    addr: &str,
    root: Option<PathBuf>,
    sec: Arc<HostSecurity>,
    requests: Sender<WebRequest>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<(JoinHandle<()>, SocketAddr)> {
    let listener = std::net::TcpListener::bind(addr)
        .map_err(|e| anyhow::anyhow!("web server on {addr}: {e}"))?;
    let local = listener.local_addr()?;
    let t = http::serve(listener, stop, move |head, body, from| {
        handle(head, body, from, root.as_deref(), &sec, &requests)
    })?;
    Ok((t, local))
}

/// `GET /api/info`: who the host is and whether it shows a PIN.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub(crate) struct Info<'a> {
    pub name: &'a str,
    pub pairing: bool,
}

/// `POST /api/connect`: the PIN the host shows and the browser's offer.
#[derive(Debug, Deserialize)]
pub(crate) struct ConnectRequest {
    pub pin: String,
    pub offer: SdpOffer,
}

/// The answer to a connect: our SDP and the stream's size and rate.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub(crate) struct Connected {
    #[cfg_attr(feature = "ts", ts(type = "RTCSessionDescriptionInit"))]
    pub answer: SdpAnswer,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
}

/// Any request turned away.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub(crate) struct Refused {
    pub error: WebError,
}

fn json_response(status: u16, v: &impl Serialize) -> Response {
    let body = serde_json::to_vec(v).expect("plain data");
    Response::new(status, "application/json", body).with("Cache-Control", "no-store")
}

fn refused(e: WebError) -> Response {
    refused_with(e.status(), e)
}

fn refused_with(status: u16, error: WebError) -> Response {
    json_response(status, &Refused { error })
}

/// Whether a request's `Host` names this machine: an IP address,
/// `localhost`, or the machine's name (alone or with a domain). Anything
/// else is a page somewhere else whose name was pointed at us (DNS
/// rebinding) and must not use the API.
pub(crate) fn host_allowed(host: Option<&str>, machine: &str) -> bool {
    let Some(host) = host else {
        return false;
    };
    // Without the port: "[::1]:47800", "192.168.1.5:47800", "zentrale".
    let name = match host.strip_prefix('[') {
        Some(v6) => {
            return v6
                .split(']')
                .next()
                .is_some_and(|a| a.parse::<IpAddr>().is_ok());
        }
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    if name.parse::<IpAddr>().is_ok() || name.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // The machine's name may come with its domain ("bazzite.fritz.box"),
    // the browser may use either: the first labels must match.
    let first = |n: &str| n.split('.').next().unwrap_or_default().to_ascii_lowercase();
    !first(machine).is_empty() && first(name) == first(machine)
}

/// Whether a POST may be a page's own: JSON (a form or a plain-text post
/// from another site cannot set that without asking first), and from the
/// page's own origin where the browser says it.
pub(crate) fn same_origin_post(head: &Head) -> bool {
    let json = head.header("content-type").is_some_and(|t| {
        t.split(';')
            .next()
            .is_some_and(|t| t.trim() == "application/json")
    });
    let origin_ok = match (head.header("origin"), head.header("host")) {
        (None, _) => true,
        (Some(origin), Some(host)) => origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
            .is_some_and(|o| o.eq_ignore_ascii_case(host)),
        (Some(_), None) => false,
    };
    json && origin_ok
}

/// Headers on every answer: no sniffing, no framing, no referrer, and a
/// content policy that allows only the page's own scripts.
fn secured(r: Response) -> Response {
    r.with("X-Content-Type-Options", "nosniff")
        .with("Referrer-Policy", "no-referrer")
        .with("X-Frame-Options", "DENY")
        .with(
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data: blob:; object-src 'none'; base-uri 'none'; \
             frame-ancestors 'none'",
        )
}

fn handle(
    head: &Head,
    body: &[u8],
    from: SocketAddr,
    root: Option<&Path>,
    sec: &HostSecurity,
    requests: &Sender<WebRequest>,
) -> Response {
    secured(route(head, body, from, root, sec, requests))
}

fn route(
    head: &Head,
    body: &[u8],
    from: SocketAddr,
    root: Option<&Path>,
    sec: &HostSecurity,
    requests: &Sender<WebRequest>,
) -> Response {
    if head.path.starts_with("/api/") && !host_allowed(head.header("host"), sec.name()) {
        return refused(WebError::WrongHost);
    }
    match (head.method, head.path.as_str()) {
        (Method::Get, "/api/info") => json_response(
            200,
            &Info {
                name: sec.name(),
                pairing: sec.pairing_remaining().is_some(),
            },
        ),
        (Method::Post, "/api/connect") if !same_origin_post(head) => refused(WebError::Forbidden),
        (Method::Post, "/api/connect") => connect(body, from, requests),
        (Method::Get | Method::Head, path) => serve_file(root, path),
        _ => refused(WebError::NotFound),
    }
}

/// How long a browser waits for the session to start.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

fn connect(body: &[u8], browser: SocketAddr, requests: &Sender<WebRequest>) -> Response {
    let Ok(ConnectRequest { pin, offer }) = serde_json::from_slice(body) else {
        return refused(WebError::BadRequest);
    };
    let Ok(local_ip) = local_ip_towards(browser.ip()) else {
        return refused(WebError::Failed);
    };
    let (reply, answer) = bounded(1);
    let req = WebRequest {
        pin,
        offer,
        browser,
        local_ip,
        reply,
    };
    if requests.send(req).is_err() {
        return refused_with(503, WebError::Failed);
    }
    match answer.recv_timeout(CONNECT_TIMEOUT) {
        Ok(Ok(a)) => json_response(
            200,
            &Connected {
                answer: a.answer,
                width: a.params.width,
                height: a.params.height,
                fps: a.params.fps,
            },
        ),
        Ok(Err(e)) => refused(e),
        Err(_) => refused_with(504, WebError::Failed),
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// The file under `root` for a URL path; app routes (no file extension)
/// get `index.html`. Never anything outside `root`.
pub(crate) fn resolve_file(root: &Path, url_path: &str) -> Option<PathBuf> {
    let rel = Path::new(url_path.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let file = root.join(rel);
    if file.is_file() {
        return Some(file);
    }
    rel.extension()
        .is_none()
        .then(|| root.join("index.html"))
        .filter(|f| f.is_file())
}

fn serve_file(root: Option<&Path>, path: &str) -> Response {
    let Some(file) = root.and_then(|r| resolve_file(r, path)) else {
        let msg = if root.is_none() {
            "Der Web-Viewer ist auf diesem Host nicht installiert (--web-root)."
        } else {
            "Nicht gefunden."
        };
        return Response::new(404, "text/plain; charset=utf-8", msg.as_bytes().to_vec());
    };
    let opened = std::fs::File::open(&file).and_then(|f| Ok((f.metadata()?.len(), f)));
    let Ok((len, f)) = opened else {
        return Response::new(500, "text/plain", Vec::new());
    };
    // Built assets carry a hash in their name: cache them for good.
    let cache = if file.parent().is_some_and(|p| p.ends_with("assets")) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Response {
        status: 200,
        headers: vec![
            ("Content-Type", content_type(&file).to_owned()),
            ("Cache-Control", cache.to_owned()),
        ],
        body: Body::File(f, len),
    }
}

/// Checks a browser's PIN against the open pairing; a right PIN is used
/// up (pairing closes).
pub(crate) fn check_pin(sec: &HostSecurity, pin: &str, from: IpAddr) -> Result<(), WebError> {
    match sec.pairing_attempt(from) {
        Ok((expected, _)) => {
            let ok = expected.len() == pin.len()
                && expected
                    .bytes()
                    .zip(pin.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0;
            if ok {
                sec.close_pairing();
                Ok(())
            } else {
                Err(WebError::WrongPin)
            }
        }
        Err(fernsicht_proto::RejectReason::TooManyAttempts) => Err(WebError::TooManyAttempts),
        Err(_) => Err(WebError::PairingClosed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_stay_inside_the_root() {
        let root = std::env::temp_dir().join(format!("fernsicht-web-{}", std::process::id()));
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("index.html"), "<html>").unwrap();
        std::fs::write(root.join("assets/app-1234.js"), "x").unwrap();
        assert_eq!(resolve_file(&root, "/"), Some(root.join("index.html")));
        assert_eq!(
            resolve_file(&root, "/assets/app-1234.js"),
            Some(root.join("assets/app-1234.js"))
        );
        // App routes get the page; missing files with an extension do not.
        assert_eq!(
            resolve_file(&root, "/session/abc"),
            Some(root.join("index.html"))
        );
        assert_eq!(resolve_file(&root, "/assets/gone.js"), None);
        for evil in ["/../etc/passwd", "/assets/../../x", "/./index.html"] {
            assert_eq!(resolve_file(&root, evil), None, "{evil}");
        }
        // A doubled slash stays inside the root too.
        assert_eq!(
            resolve_file(&root, "//etc/passwd"),
            Some(root.join("index.html"))
        );
        assert_eq!(content_type(Path::new("a.js")), "text/javascript");
        assert_eq!(
            content_type(Path::new("a.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("a")), "application/octet-stream");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_api_answers_only_to_this_machines_names() {
        for ok in [
            "192.168.178.87:47800",
            "[fe80::1]:47800",
            "[::1]",
            "localhost:47800",
            "zentrale:47800",
            "Zentrale.fritz.box:47800",
            "zentrale.local",
        ] {
            assert!(host_allowed(Some(ok), "zentrale"), "{ok}");
        }
        // DNS rebinding: someone else's name pointed at us.
        for bad in [
            "evil.example:47800",
            "zentrale-evil.example",
            "[not-an-ip]:1",
            "",
        ] {
            assert!(!host_allowed(Some(bad), "zentrale"), "{bad}");
        }
        assert!(!host_allowed(None, "zentrale"));
        // A machine whose name has its domain.
        assert!(host_allowed(
            Some("bazzite.fritz.box:47800"),
            "bazzite.fritz.box"
        ));
        assert!(host_allowed(Some("bazzite:47800"), "bazzite.fritz.box"));
        assert!(!host_allowed(Some("evil.fritz.box"), "bazzite.fritz.box"));
    }

    #[test]
    fn only_the_pages_own_json_posts_connect() {
        let head = |headers: &[(&str, &str)]| Head {
            method: Method::Post,
            path: "/api/connect".into(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
        };
        let json = ("content-type", "application/json");
        let host = ("host", "192.168.178.87:47800");
        assert!(same_origin_post(&head(&[json, host])));
        assert!(same_origin_post(&head(&[
            ("content-type", "application/json; charset=utf-8"),
            host,
            ("origin", "http://192.168.178.87:47800"),
        ])));
        // A plain-text post from another site (no preflight needed).
        assert!(!same_origin_post(&head(&[
            ("content-type", "text/plain"),
            host
        ])));
        assert!(!same_origin_post(&head(&[
            json,
            host,
            ("origin", "http://evil.example")
        ])));
    }

    #[test]
    fn pins_are_checked_once() {
        let sec = HostSecurity::new(
            fernsicht_secure::Identity::generate(),
            "zentrale",
            fernsicht_secure::Trusted::default(),
            None,
        );
        let (me, other) = (
            IpAddr::from([192, 168, 1, 5]),
            IpAddr::from([192, 168, 1, 6]),
        );
        assert_eq!(check_pin(&sec, "123456", me), Err(WebError::PairingClosed));
        sec.open_pairing("123456");
        assert_eq!(check_pin(&sec, "654321", me), Err(WebError::WrongPin));
        assert_eq!(check_pin(&sec, "12345", me), Err(WebError::WrongPin));
        assert_eq!(check_pin(&sec, "123456", me), Ok(()));
        assert_eq!(
            check_pin(&sec, "123456", me),
            Err(WebError::PairingClosed),
            "used up"
        );
        // Three wrong from one address turn that one away, not the others.
        sec.open_pairing("111111");
        for _ in 0..3 {
            assert_eq!(check_pin(&sec, "000000", me), Err(WebError::WrongPin));
        }
        assert_eq!(
            check_pin(&sec, "111111", me),
            Err(WebError::TooManyAttempts)
        );
        assert_eq!(check_pin(&sec, "111111", other), Ok(()));
        assert_eq!(
            serde_json::to_value(WebError::WrongPin).unwrap(),
            "wrong-pin"
        );
        assert_eq!(WebError::Failed.status(), 500);
    }
}
