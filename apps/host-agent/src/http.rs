//! The small HTTP/1.1 server the web viewer needs: its page, `GET
//! /api/info`, `POST /api/connect`. It runs in a root service and faces
//! the LAN, so everything is bounded: the request head, the body, the time
//! a connection may take and how many run at once. One request per
//! connection (`Connection: close`).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Largest request head (request line and headers).
pub const MAX_HEAD: usize = 8 * 1024;
/// Most headers in a request.
pub const MAX_HEADERS: usize = 32;
/// Largest request body (an SDP offer is a few KiB).
pub const MAX_BODY: usize = 64 * 1024;
/// How long a connection may take to send its request.
pub const REQUEST_TIME: Duration = Duration::from_secs(10);
/// Connections served at once; more wait in the kernel's backlog.
pub const MAX_CONNECTIONS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
}

/// A request's head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub method: Method,
    /// The path without the query.
    pub path: String,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Why a request is refused; each maps to a status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Malformed.
    BadRequest,
    /// Head or body too large.
    TooLarge,
    /// A method other than GET, HEAD, POST.
    Method,
    /// Chunked bodies are not taken.
    LengthRequired,
}

impl Refused {
    pub fn status(self) -> u16 {
        match self {
            Refused::BadRequest => 400,
            Refused::TooLarge => 413,
            Refused::Method => 405,
            Refused::LengthRequired => 411,
        }
    }
}

/// The head at the start of `buf`, and its length with the blank line;
/// `Ok(None)` while it is incomplete (and still within the limits).
pub fn parse_head(buf: &[u8]) -> Result<Option<(Head, usize)>, Refused> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_HEAD {
            Err(Refused::TooLarge)
        } else {
            Ok(None)
        };
    };
    if end + 4 > MAX_HEAD {
        return Err(Refused::TooLarge);
    }
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| Refused::BadRequest)?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Refused::BadRequest);
    };
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") || !target.starts_with('/') {
        return Err(Refused::BadRequest);
    }
    let method = match method {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        "POST" => Method::Post,
        _ => return Err(Refused::Method),
    };
    let mut headers = Vec::new();
    for line in lines {
        if headers.len() == MAX_HEADERS {
            return Err(Refused::TooLarge);
        }
        let (name, value) = line.split_once(':').ok_or(Refused::BadRequest)?;
        if name.is_empty() || name.contains(|c: char| c.is_whitespace()) {
            return Err(Refused::BadRequest);
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let path = target.split('?').next().unwrap_or("/").to_owned();
    Ok(Some((
        Head {
            method,
            path,
            headers,
        },
        end + 4,
    )))
}

/// The body length a head announces: none for GET/HEAD, a bounded
/// `Content-Length` for POST.
pub fn body_length(head: &Head) -> Result<usize, Refused> {
    if head.header("transfer-encoding").is_some() {
        return Err(Refused::LengthRequired);
    }
    match (head.method, head.header("content-length")) {
        (Method::Post, None) => Err(Refused::LengthRequired),
        (_, None) => Ok(0),
        (_, Some(v)) => {
            let n: usize = v.parse().map_err(|_| Refused::BadRequest)?;
            if n > MAX_BODY {
                Err(Refused::TooLarge)
            } else {
                Ok(n)
            }
        }
    }
}

/// A response: status, headers, body (`Body::File` is streamed).
pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Body,
}

pub enum Body {
    Bytes(Vec<u8>),
    File(std::fs::File, u64),
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", content_type.to_owned())],
            body: Body::Bytes(body),
        }
    }

    pub fn with(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

/// Writes `r`; for HEAD without its body.
fn write_response(stream: &mut TcpStream, r: Response, head_only: bool) -> io::Result<()> {
    let len = match &r.body {
        Body::Bytes(b) => b.len() as u64,
        Body::File(_, n) => *n,
    };
    let mut out = format!("HTTP/1.1 {} {}\r\n", r.status, reason(r.status));
    for (name, value) in &r.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {len}\r\nConnection: close\r\n\r\n"
    ));
    stream.write_all(out.as_bytes())?;
    if head_only {
        return Ok(());
    }
    match r.body {
        Body::Bytes(b) => stream.write_all(&b),
        Body::File(mut f, n) => io::copy(&mut (&mut f).take(n), stream).map(|_| ()),
    }
}

/// Reads one request within [`REQUEST_TIME`] and the limits.
fn read_request(stream: &mut TcpStream) -> Result<(Head, Vec<u8>), Refused> {
    let deadline = Instant::now() + REQUEST_TIME;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let (head, used) = loop {
        if let Some(h) = parse_head(&buf)? {
            break h;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return Err(Refused::BadRequest);
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(Refused::BadRequest),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let len = body_length(&head)?;
    let mut body = buf.split_off(used);
    while body.len() < len {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return Err(Refused::BadRequest);
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(Refused::BadRequest),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    body.truncate(len);
    Ok((head, body))
}

/// Serves `handle` on `listener` until `stop`, each connection on its own
/// thread, at most [`MAX_CONNECTIONS`] at once.
pub fn serve(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    handle: impl Fn(&Head, &[u8], SocketAddr) -> Response + Send + Sync + 'static,
) -> io::Result<JoinHandle<()>> {
    listener.set_nonblocking(true)?;
    let handle = Arc::new(handle);
    let running = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("web".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (mut stream, from) = match listener.accept() {
                    Ok(c) => c,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(e) => {
                        log::debug!("web: accept: {e}");
                        continue;
                    }
                };
                if running.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                    // Busy: told so instead of piling up threads.
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                    let _ = write_response(
                        &mut stream,
                        Response::new(503, "text/plain", Vec::new()),
                        false,
                    );
                    continue;
                }
                running.fetch_add(1, Ordering::AcqRel);
                let (handle, done) = (handle.clone(), running.clone());
                let spawned =
                    std::thread::Builder::new()
                        .name("web-conn".into())
                        .spawn(move || {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_write_timeout(Some(REQUEST_TIME));
                            let response = match read_request(&mut stream) {
                                Ok((head, body)) => {
                                    let head_only = head.method == Method::Head;
                                    let r = handle(&head, &body, from);
                                    let _ = write_response(&mut stream, r, head_only);
                                    None
                                }
                                Err(refused) => Some(refused),
                            };
                            if let Some(refused) = response {
                                let _ = write_response(
                                    &mut stream,
                                    Response::new(refused.status(), "text/plain", Vec::new()),
                                    false,
                                );
                            }
                            done.fetch_sub(1, Ordering::AcqRel);
                        });
                if spawned.is_err() {
                    running.fetch_sub(1, Ordering::AcqRel);
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_head_is_read() {
        let req = b"POST /api/connect?x=1 HTTP/1.1\r\nHost: zentrale:47800\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        let (head, used) = parse_head(req).unwrap().unwrap();
        assert_eq!(head.method, Method::Post);
        assert_eq!(head.path, "/api/connect");
        assert_eq!(head.header("content-type"), Some("application/json"));
        assert_eq!(head.header("host"), Some("zentrale:47800"));
        assert_eq!(&req[used..], b"{}");
        assert_eq!(body_length(&head), Ok(2));
    }

    #[test]
    fn an_incomplete_head_waits_within_the_limit() {
        assert_eq!(parse_head(b"GET / HTTP/1.1\r\nHost: x"), Ok(None));
        // A header line that never ends: refused once past the limit.
        let mut endless = b"GET / HTTP/1.1\r\nX-Long: ".to_vec();
        endless.resize(MAX_HEAD + 1, b'a');
        assert_eq!(parse_head(&endless), Err(Refused::TooLarge));
    }

    #[test]
    fn too_many_headers_and_bad_lines_are_refused() {
        let many: String = (0..=MAX_HEADERS).map(|i| format!("X-{i}: y\r\n")).collect();
        let req = format!("GET / HTTP/1.1\r\n{many}\r\n");
        assert_eq!(parse_head(req.as_bytes()), Err(Refused::TooLarge));
        for bad in [
            &b"GET / HTTP/1.1 extra\r\n\r\n"[..],
            b"GET http://x/ HTTP/1.1\r\n\r\n",
            b"GET / HTTP/2\r\n\r\n",
            b"GET / HTTP/1.1\r\nno colon\r\n\r\n",
            b"GET / HTTP/1.1\r\nbad name: x\r\n\r\n",
            b"\xff / HTTP/1.1\r\n\r\n",
        ] {
            assert_eq!(parse_head(bad), Err(Refused::BadRequest), "{bad:?}");
        }
        assert_eq!(parse_head(b"PUT / HTTP/1.1\r\n\r\n"), Err(Refused::Method));
    }

    #[test]
    fn bodies_are_bounded_and_must_have_a_length() {
        let head = |extra: &str| {
            parse_head(format!("POST /api/connect HTTP/1.1\r\n{extra}\r\n").as_bytes())
                .unwrap()
                .unwrap()
                .0
        };
        assert_eq!(body_length(&head("")), Err(Refused::LengthRequired));
        assert_eq!(
            body_length(&head("Transfer-Encoding: chunked\r\n")),
            Err(Refused::LengthRequired)
        );
        assert_eq!(
            body_length(&head(&format!("Content-Length: {}\r\n", MAX_BODY + 1))),
            Err(Refused::TooLarge)
        );
        assert_eq!(
            body_length(&head("Content-Length: x\r\n")),
            Err(Refused::BadRequest)
        );
    }

    #[test]
    fn a_slow_client_is_cut_off_and_others_are_served() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let t = serve(listener, stop.clone(), |head, _, _| {
            Response::new(200, "text/plain", head.path.clone().into_bytes())
        })
        .unwrap();
        // One that sends half a head and waits.
        let mut slow = TcpStream::connect(addr).unwrap();
        slow.write_all(b"GET /slow HTTP/1.1\r\n").unwrap();
        // Another is answered meanwhile.
        let mut fast = TcpStream::connect(addr).unwrap();
        fast.write_all(b"GET /fast HTTP/1.1\r\n\r\n").unwrap();
        let mut answer = String::new();
        fast.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(answer.ends_with("/fast"));
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }
}
