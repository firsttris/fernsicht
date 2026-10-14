//! Talking to a rendezvous service (the edge: TCP, TLS, WebSockets): a
//! host's standing connection, and a client's one request.
//!
//! The messages are those of [`crate::signal`]; this module only moves
//! them. Blocking, with timeouts: the host runs it on a thread of its own,
//! the client before the session starts.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use fernsicht_secure::{Identity, PublicKey};

use crate::signal::{
    self, ClientToService, ErrorCode, HostToService, IceServer, MAX_MESSAGE, ServiceToClient,
    ServiceToHost,
};

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// How long connecting (TCP, TLS, the WebSocket handshake) may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why talking to the service failed.
#[derive(Debug)]
pub enum Error {
    /// The address is not one we can use.
    BadUrl(String),
    /// The network, TLS or the WebSocket failed.
    Io(String),
    /// The service said no.
    Service(ErrorCode, String),
    /// It sent something that is not its protocol.
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::BadUrl(u) => write!(f, "not a rendezvous service address: {u}"),
            Error::Io(e) => write!(f, "rendezvous service: {e}"),
            Error::Service(code, m) => write!(f, "rendezvous service: {code:?}: {m}"),
            Error::Protocol(e) => write!(f, "rendezvous service: unexpected answer: {e}"),
        }
    }
}

impl std::error::Error for Error {}

fn io(e: impl std::fmt::Display) -> Error {
    Error::Io(e.to_string())
}

/// The WebSocket address for `path` on the service at `base`: `https://`
/// becomes `wss://`, `http://` (tests, a service on the same machine)
/// becomes `ws://`; a path in `base` is kept in front.
pub fn ws_url(base: &str, path: &str) -> Result<String, Error> {
    let base = base.trim().trim_end_matches('/');
    let rest = |p: &str| base.strip_prefix(p);
    let (scheme, rest) = if let Some(r) = rest("https://") {
        ("wss", r)
    } else if let Some(r) = rest("wss://") {
        ("wss", r)
    } else if let Some(r) = rest("http://") {
        ("ws", r)
    } else if let Some(r) = rest("ws://") {
        ("ws", r)
    } else {
        return Err(Error::BadUrl(base.to_owned()));
    };
    if rest.is_empty() || rest.contains(['?', '#', ' ']) {
        return Err(Error::BadUrl(base.to_owned()));
    }
    Ok(format!("{scheme}://{rest}{path}"))
}

/// Whether `url` is a usable service address (for settings).
pub fn is_service_url(url: &str) -> bool {
    url.len() <= fernsicht_proto::MAX_RENDEZVOUS_URL && ws_url(url, "/").is_ok()
}

/// `host:port` of a WebSocket URL.
fn authority(url: &str) -> Result<(String, u16), Error> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| Error::BadUrl(url.into()))?;
    let hostport = rest.split('/').next().unwrap_or("");
    let default = if scheme == "wss" { 443 } else { 80 };
    match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(':') && !p.contains(']') => Ok((
            h.trim_start_matches('[').trim_end_matches(']').to_owned(),
            p.parse().map_err(|_| Error::BadUrl(url.into()))?,
        )),
        _ => Ok((
            hostport
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
            default,
        )),
    }
}

fn open(url: &str) -> Result<Socket, Error> {
    let (host, port) = authority(url)?;
    let addrs: Vec<_> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(io)?
        .collect();
    let stream = addrs
        .iter()
        .find_map(|a| TcpStream::connect_timeout(a, CONNECT_TIMEOUT).ok())
        .ok_or_else(|| Error::Io(format!("cannot reach {host}:{port}")))?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT)).map_err(io)?;
    stream
        .set_write_timeout(Some(CONNECT_TIMEOUT))
        .map_err(io)?;
    let _ = stream.set_nodelay(true);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (socket, _) = tungstenite::client_tls_with_config(url, stream, Some(config), None)
        .map_err(|e| Error::Io(format!("{url}: {e}")))?;
    Ok(socket)
}

fn set_read_timeout(ws: &Socket, t: Duration) {
    let t = Some(t.max(Duration::from_millis(1)));
    let _ = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(t),
        MaybeTlsStream::Rustls(s) => s.get_ref().set_read_timeout(t),
        _ => Ok(()),
    };
}

fn send<T: Serialize>(ws: &mut Socket, msg: &T) -> Result<(), Error> {
    let text = serde_json::to_string(msg).expect("serializable");
    ws.send(Message::text(text)).map_err(io)
}

/// The next message, or `None` if none came within the socket's read
/// timeout.
fn receive<T: DeserializeOwned>(ws: &mut Socket) -> Result<Option<T>, Error> {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                return serde_json::from_str(t.as_str())
                    .map(Some)
                    .map_err(|e| Error::Protocol(e.to_string()));
            }
            Ok(Message::Close(_)) => return Err(Error::Io("closed by the service".into())),
            // Pings are answered by tungstenite on the next write or read.
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(io(e)),
        }
    }
}

/// An offer the service passed on to the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingOffer {
    pub connection: u32,
    pub offer: String,
    pub ice_servers: Vec<IceServer>,
}

/// A host's standing connection: registered, proven, waiting for offers.
pub struct HostConnection {
    ws: Socket,
    /// The service hands out TURN credentials.
    pub relay: bool,
    last_ping: Instant,
}

/// How often the host pings the service (connections idle for longer are
/// closed by some proxies).
const PING_EVERY: Duration = Duration::from_secs(30);

impl HostConnection {
    /// Registers `identity` with the service at `base` and proves its key.
    pub fn open(base: &str, identity: &Identity) -> Result<Self, Error> {
        let id = signal::rendezvous_id(&identity.public);
        let mut ws = open(&ws_url(base, &format!("{}{id}", signal::HOST_PATH))?)?;
        send(
            &mut ws,
            &HostToService::Register {
                key: signal::hex(&identity.public.0),
            },
        )?;
        let (ephemeral, nonce) = match receive::<ServiceToHost>(&mut ws)? {
            Some(ServiceToHost::Challenge { ephemeral, nonce }) => (ephemeral, nonce),
            Some(ServiceToHost::Error { code, message }) => {
                return Err(Error::Service(code, message));
            }
            other => return Err(Error::Protocol(format!("{other:?} instead of a challenge"))),
        };
        let ephemeral =
            signal::unhex32(&ephemeral).ok_or_else(|| Error::Protocol("challenge key".into()))?;
        let nonce = signal::unhex(&nonce)
            .filter(|n| (16..=64).contains(&n.len()))
            .ok_or_else(|| Error::Protocol("challenge nonce".into()))?;
        let mac = signal::prove(identity, &PublicKey(ephemeral), &nonce)
            .ok_or_else(|| Error::Protocol("unusable challenge key".into()))?;
        send(
            &mut ws,
            &HostToService::Proof {
                mac: signal::hex(&mac),
            },
        )?;
        match receive::<ServiceToHost>(&mut ws)? {
            Some(ServiceToHost::Registered { relay }) => Ok(Self {
                ws,
                relay,
                last_ping: Instant::now(),
            }),
            Some(ServiceToHost::Error { code, message }) => Err(Error::Service(code, message)),
            other => Err(Error::Protocol(format!("{other:?} instead of registered"))),
        }
    }

    /// Waits up to `wait` for the next offer; pings the service now and
    /// then. An error means the connection is gone.
    pub fn next_offer(&mut self, wait: Duration) -> Result<Option<IncomingOffer>, Error> {
        if self.last_ping.elapsed() >= PING_EVERY {
            self.ws.send(Message::Ping(Vec::new().into())).map_err(io)?;
            self.last_ping = Instant::now();
        }
        set_read_timeout(&self.ws, wait);
        match receive::<ServiceToHost>(&mut self.ws)? {
            None => Ok(None),
            Some(ServiceToHost::Offer {
                connection,
                offer,
                ice_servers,
            }) => Ok(Some(IncomingOffer {
                connection,
                offer,
                ice_servers,
            })),
            Some(ServiceToHost::Error { code, message }) => Err(Error::Service(code, message)),
            Some(other) => Err(Error::Protocol(format!("{other:?}"))),
        }
    }

    pub fn answer(&mut self, connection: u32, answer: String) -> Result<(), Error> {
        send(&mut self.ws, &HostToService::Answer { connection, answer })
    }

    pub fn refuse(&mut self, connection: u32) -> Result<(), Error> {
        send(&mut self.ws, &HostToService::Refuse { connection })
    }

    pub fn close(mut self) {
        let _ = self.ws.close(None);
        let _ = self.ws.flush();
    }
}

/// A client's request: the sealed `offer` to the host with key `host` at
/// the service at `base`; the sealed answer.
pub fn connect(base: &str, host: &PublicKey, offer: String) -> Result<String, Error> {
    let id = signal::rendezvous_id(host);
    let mut ws = open(&ws_url(base, &format!("{}{id}", signal::CONNECT_PATH))?)?;
    send(&mut ws, &ClientToService::Connect { offer })?;
    // The service holds the offer up to its own timeout; a little longer
    // here so its error arrives rather than ours.
    set_read_timeout(
        &ws,
        Duration::from_millis(signal::ANSWER_TIMEOUT_MS) + Duration::from_secs(5),
    );
    let answer = match receive::<ServiceToClient>(&mut ws)? {
        Some(ServiceToClient::Answer { answer }) => Ok(answer),
        Some(ServiceToClient::Error { code, message }) => Err(Error::Service(code, message)),
        None => Err(Error::Service(ErrorCode::Timeout, "no answer".into())),
    };
    let _ = ws.close(None);
    let _ = ws.flush();
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_addresses_become_websocket_addresses() {
        assert_eq!(
            ws_url("https://r.someone.workers.dev", "/v1/host/ab").unwrap(),
            "wss://r.someone.workers.dev/v1/host/ab"
        );
        assert_eq!(
            ws_url("http://127.0.0.1:8080/", "/v1/connect/ab").unwrap(),
            "ws://127.0.0.1:8080/v1/connect/ab"
        );
        assert_eq!(
            ws_url("https://example.org/fernsicht", "/v1/host/x").unwrap(),
            "wss://example.org/fernsicht/v1/host/x"
        );
        for bad in ["", "ftp://x", "https://", "x.workers.dev", "https://a?b"] {
            assert!(ws_url(bad, "/").is_err(), "{bad}");
        }
        assert!(is_service_url("https://r.someone.workers.dev"));
        assert!(!is_service_url(&format!("https://{}", "x".repeat(300))));
    }

    #[test]
    fn authorities_and_default_ports() {
        assert_eq!(authority("wss://a.b/x").unwrap(), ("a.b".into(), 443));
        assert_eq!(
            authority("ws://127.0.0.1:8080/x").unwrap(),
            ("127.0.0.1".into(), 8080)
        );
        assert_eq!(authority("ws://[::1]:9/x").unwrap(), ("::1".into(), 9));
        assert_eq!(authority("wss://[::1]/x").unwrap(), ("::1".into(), 443));
    }
}
