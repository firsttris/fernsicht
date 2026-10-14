//! The rendezvous service, self-hosted (`docs/internet.md`): hosts keep a
//! WebSocket open and prove their key; clients send a sealed offer and get
//! the host's sealed answer. The service passes them on and sees nothing
//! of what they say. A Cloudflare Worker does the same
//! (`services/rendezvous-worker`).
//!
//! One thread per connection: a home's few devices, not a crowd. Behind a
//! reverse proxy for HTTPS (Caddy, nginx).

mod state;
pub mod turn;

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, unbounded};
use fernsicht_internet::signal::{
    self, ClientToService, ErrorCode, HostToService, MAX_MESSAGE, OFFERS_PER_MINUTE,
    ServiceToClient, ServiceToHost,
};
use fernsicht_secure::PublicKey;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

pub use state::{RateLimit, State, ToHost};

/// What the service is started with.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// TURN credentials to hand out with each offer.
    pub turn: Option<turn::TurnConfig>,
}

/// Most connections at once.
const MAX_CONNECTIONS: usize = 256;
/// How long a new connection may take to say what it wants.
const FIRST_MESSAGE: Duration = Duration::from_secs(10);
/// A host that sends nothing (not even a ping) for this long is gone.
const HOST_IDLE: Duration = Duration::from_secs(90);

type Ws = WebSocket<TcpStream>;

/// Where a WebSocket's path leads.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Host(String),
    Connect(String),
    NotFound,
}

/// The route for a request path (a prefix in front, from a reverse proxy,
/// is allowed).
pub fn route(path: &str) -> Route {
    let tail = |prefix: &str| {
        let i = path.find(prefix)?;
        let id = &path[i + prefix.len()..];
        signal::is_rendezvous_id(id).then(|| id.to_owned())
    };
    if let Some(id) = tail(signal::HOST_PATH) {
        Route::Host(id)
    } else if let Some(id) = tail(signal::CONNECT_PATH) {
        Route::Connect(id)
    } else {
        Route::NotFound
    }
}

/// Serves on `listener` until `stop`.
pub fn serve(
    listener: TcpListener,
    cfg: Config,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    listener.set_nonblocking(true)?;
    let state = Arc::new(Mutex::new(State::default()));
    let open = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("rendezvous".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (stream, peer) = match listener.accept() {
                    Ok(c) => c,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(e) => {
                        log::warn!("accept: {e}");
                        continue;
                    }
                };
                if open.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    continue;
                }
                open.fetch_add(1, Ordering::Relaxed);
                let (state, cfg, open, stop) =
                    (state.clone(), cfg.clone(), open.clone(), stop.clone());
                let spawned =
                    std::thread::Builder::new()
                        .name("connection".into())
                        .spawn(move || {
                            if let Err(e) = handle(stream, peer, &state, &cfg, &stop) {
                                log::debug!("{peer}: {e}");
                            }
                            open.fetch_sub(1, Ordering::Relaxed);
                        });
                if spawned.is_err() {
                    log::warn!("no thread for {peer}");
                }
            }
        })
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn send<T: Serialize>(ws: &mut Ws, msg: &T) -> anyhow::Result<()> {
    ws.send(Message::text(serde_json::to_string(msg)?))?;
    Ok(())
}

/// The next message (or `None` within the read timeout).
fn receive<T: DeserializeOwned>(ws: &mut Ws) -> anyhow::Result<Option<T>> {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => return Ok(Some(serde_json::from_str(t.as_str())?)),
            Ok(Message::Close(_)) => anyhow::bail!("closed"),
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// The first message, within [`FIRST_MESSAGE`].
fn first<T: DeserializeOwned>(ws: &mut Ws) -> anyhow::Result<T> {
    ws.get_ref().set_read_timeout(Some(FIRST_MESSAGE))?;
    receive(ws)?.ok_or_else(|| anyhow::anyhow!("nothing said"))
}

// The handshake callback's error type is tungstenite's (an HTTP response).
#[allow(clippy::result_large_err)]
fn handle(
    stream: TcpStream,
    peer: SocketAddr,
    state: &Mutex<State>,
    cfg: &Config,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(FIRST_MESSAGE))?;
    stream.set_write_timeout(Some(FIRST_MESSAGE))?;
    let path = Arc::new(Mutex::new(String::new()));
    let seen = path.clone();
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let ws = tungstenite::accept_hdr_with_config(
        stream,
        move |req: &tungstenite::handshake::server::Request, resp| {
            *seen.lock().unwrap_or_else(|e| e.into_inner()) = req.uri().path().to_owned();
            Ok(resp)
        },
        Some(config),
    )?;
    let path = path.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match route(&path) {
        Route::Host(id) => host(ws, &id, state, cfg, stop),
        Route::Connect(id) => client(ws, peer, &id, state, cfg),
        Route::NotFound => anyhow::bail!("no such path: {path}"),
    }
}

/// A host's connection: register, prove the key, then pass offers and
/// answers until it goes.
fn host(
    mut ws: Ws,
    id: &str,
    state: &Mutex<State>,
    cfg: &Config,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let HostToService::Register { key } = first(&mut ws)? else {
        anyhow::bail!("expected register");
    };
    let key = signal::unhex32(&key).map(PublicKey);
    let Some(key) = key.filter(|k| signal::rendezvous_id(k) == id) else {
        let code = ErrorCode::BadRequest;
        send(
            &mut ws,
            &ServiceToHost::Error {
                code,
                message: "the key does not match the ID".into(),
            },
        )?;
        anyhow::bail!("key does not match its ID");
    };
    let mut secret = [0u8; 32];
    let mut nonce = [0u8; 16];
    fernsicht_secure::random(&mut secret);
    fernsicht_secure::random(&mut nonce);
    let secret = x25519_dalek::StaticSecret::from(secret);
    let ephemeral = x25519_dalek::PublicKey::from(&secret);
    send(
        &mut ws,
        &ServiceToHost::Challenge {
            ephemeral: signal::hex(ephemeral.as_bytes()),
            nonce: signal::hex(&nonce),
        },
    )?;
    let HostToService::Proof { mac } = first(&mut ws)? else {
        anyhow::bail!("expected the proof");
    };
    let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(key.0));
    let expected = signal::proof(&shared.to_bytes(), &nonce, &key);
    let proven =
        shared.was_contributory() && signal::unhex32(&mac).is_some_and(|m| same(&m, &expected));
    if !proven {
        send(
            &mut ws,
            &ServiceToHost::Error {
                code: ErrorCode::Unauthorized,
                message: "key proof failed".into(),
            },
        )?;
        anyhow::bail!("key proof failed");
    }
    let (tx, rx) = unbounded();
    let generation = lock(state).register(id, tx);
    send(
        &mut ws,
        &ServiceToHost::Registered {
            relay: cfg.turn.is_some(),
        },
    )?;
    log::info!("host {} registered", &id[..12]);
    let result = host_loop(&mut ws, (id, generation), &rx, state, stop);
    lock(state).unregister(id, generation);
    log::info!("host {} gone", &id[..12]);
    result
}

fn host_loop(
    ws: &mut Ws,
    me: (&str, u64),
    rx: &crossbeam_channel::Receiver<ToHost>,
    state: &Mutex<State>,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    ws.get_ref()
        .set_read_timeout(Some(Duration::from_millis(50)))?;
    let mut heard = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        while let Ok(m) = rx.try_recv() {
            match m {
                ToHost::Offer {
                    connection,
                    offer,
                    ice_servers,
                } => {
                    send(
                        ws,
                        &ServiceToHost::Offer {
                            connection,
                            offer,
                            ice_servers,
                        },
                    )?;
                }
                ToHost::Replaced => {
                    let message = "another connection registered with this key".into();
                    send(
                        ws,
                        &ServiceToHost::Error {
                            code: ErrorCode::Replaced,
                            message,
                        },
                    )?;
                    let _ = ws.close(None);
                    return Ok(());
                }
            }
        }
        // Pings count as life: tungstenite answers them while reading.
        match ws.read() {
            Ok(Message::Text(t)) => {
                heard = Instant::now();
                let msg: HostToService = serde_json::from_str(t.as_str())?;
                let (connection, reply) = match msg {
                    HostToService::Answer { connection, answer } => {
                        (connection, ServiceToClient::Answer { answer })
                    }
                    HostToService::Refuse { connection } => (
                        connection,
                        ServiceToClient::Error {
                            code: ErrorCode::Refused,
                            message: "the host refused the offer".into(),
                        },
                    ),
                    other => anyhow::bail!("out of turn: {other:?}"),
                };
                if !lock(state).answer(me, connection, reply) {
                    log::debug!("an answer for connection {connection}, which is not waiting");
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => heard = Instant::now(),
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e.into()),
        }
        if heard.elapsed() > HOST_IDLE {
            anyhow::bail!("silent for {HOST_IDLE:?}");
        }
    }
    Ok(())
}

/// A client's connection: one offer, one answer (or why not).
fn client(
    mut ws: Ws,
    peer: SocketAddr,
    id: &str,
    state: &Mutex<State>,
    cfg: &Config,
) -> anyhow::Result<()> {
    let ClientToService::Connect { offer } = first(&mut ws)?;
    let now = Instant::now();
    let reply = {
        let mut s = lock(state);
        let allowed = s
            .limits
            .take(&format!("ip {}", peer.ip()), OFFERS_PER_MINUTE, now)
            && s.limits.take(&format!("host {id}"), OFFERS_PER_MINUTE, now);
        if !allowed {
            Err(ErrorCode::RateLimited)
        } else {
            let ice = cfg
                .turn
                .as_ref()
                .map(|t| vec![t.credentials(unix_now())])
                .unwrap_or_default();
            let (tx, rx) = unbounded();
            s.offer(id, offer, ice, tx).map(|c| (c, rx))
        }
    };
    let message = match reply {
        Err(code) => ServiceToClient::Error {
            code,
            message: match code {
                ErrorCode::Offline => "the host is not connected to this service",
                _ => "too many offers; wait a minute",
            }
            .into(),
        },
        Ok((connection, rx)) => {
            match rx.recv_timeout(Duration::from_millis(signal::ANSWER_TIMEOUT_MS)) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    lock(state).give_up(connection);
                    ServiceToClient::Error {
                        code: ErrorCode::Timeout,
                        message: "the host did not answer".into(),
                    }
                }
            }
        }
    };
    send(&mut ws, &message)?;
    let _ = ws.close(None);
    let _ = ws.flush();
    Ok(())
}

/// Equal, compared in constant time (a MAC).
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_lead_to_hosts_and_connects() {
        let id = "a".repeat(64);
        assert_eq!(route(&format!("/v1/host/{id}")), Route::Host(id.clone()));
        assert_eq!(
            route(&format!("/fernsicht/v1/connect/{id}")),
            Route::Connect(id.clone())
        );
        assert_eq!(route("/v1/host/short"), Route::NotFound);
        assert_eq!(route(&format!("/v1/host/{id}/x")), Route::NotFound);
        assert_eq!(route("/"), Route::NotFound);
    }
}
