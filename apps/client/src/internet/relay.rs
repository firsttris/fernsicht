//! A session through a TURN relay: a thread between the session's socket
//! and the relay, so the network thread stays as it is (a connected UDP
//! socket). It wraps what the session sends in ChannelData, unwraps what
//! comes back, and keeps the allocation and its channel fresh.
//!
//! The relay costs a hop through the kernel's loopback (tens of µs) on top
//! of the relay's milliseconds.

use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fernsicht_internet::turn::{Event, TurnClient};
use fernsicht_net::socket::for_socket;

/// The relay's thread; stops (and releases the allocation) when dropped.
pub struct Relay {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts relaying between a new session socket (returned, connected) and
/// `peer` through `turn` on `socket`.
pub fn spawn(
    socket: UdpSocket,
    turn: TurnClient,
    peer: SocketAddr,
) -> std::io::Result<(UdpSocket, Relay)> {
    let inner = UdpSocket::bind("127.0.0.1:0")?;
    let session = fernsicht_net::socket::bind_udp("127.0.0.1:0")?;
    inner.connect(session.local_addr()?)?;
    session.connect(inner.local_addr()?)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("relay".into())
        .spawn(move || run(&socket, &inner, turn, peer, &flag))?;
    Ok((
        session,
        Relay {
            stop,
            thread: Some(thread),
        },
    ))
}

/// Waits up to `ms` for either socket; which are readable.
fn poll2(a: &UdpSocket, b: &UdpSocket, ms: i32) -> (bool, bool) {
    let mut fds = [
        libc::pollfd {
            fd: a.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: b.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: a valid array of two pollfd.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
    if n <= 0 {
        return (false, false);
    }
    (fds[0].revents != 0, fds[1].revents != 0)
}

fn flush(socket: &UdpSocket, turn: &mut TurnClient) {
    let server = for_socket(socket, turn.server());
    while let Some(m) = turn.poll_transmit() {
        let _ = socket.send_to(&m, server);
    }
}

fn run(
    socket: &UdpSocket,
    inner: &UdpSocket,
    mut turn: TurnClient,
    peer: SocketAddr,
    stop: &AtomicBool,
) {
    let server = fernsicht_internet::punch::canonical(turn.server());
    let mut buf = vec![0u8; 65_536];
    let mut out = vec![0u8; 65_536];
    while !stop.load(Ordering::Relaxed) {
        let wait = turn.next_tick().map_or(50, |t| {
            t.saturating_duration_since(Instant::now())
                .as_millis()
                .min(50) as i32
        });
        let (from_relay, from_session) = poll2(socket, inner, wait);
        if from_session && let Ok(n) = inner.recv(&mut buf) {
            // Per packet: no allocation, the buffers are the thread's.
            match turn.wrap(peer, &buf[..n], &mut out) {
                Some(m) => {
                    let _ = socket.send_to(&out[..m], for_socket(socket, server));
                }
                // Until its channel is bound (a moment after the start; the
                // indication allocates, only then).
                None => {
                    let ind = turn.send_indication(peer, &buf[..n]);
                    let _ = socket.send_to(&ind, for_socket(socket, server));
                }
            }
        }
        if from_relay
            && let Ok((n, from)) = socket.recv_from(&mut buf)
            && fernsicht_internet::punch::canonical(from) == server
        {
            match turn.handle(&buf[..n], Instant::now()) {
                Event::Data { peer: p, data } if p == peer => {
                    let _ = inner.send(data);
                }
                Event::Failed(why) => {
                    log::warn!("relay: {why}");
                    return;
                }
                _ => {}
            }
        }
        turn.tick(Instant::now());
        flush(socket, &mut turn);
        if let Some(why) = turn.failure() {
            log::warn!("relay: {why}");
            return;
        }
    }
    turn.close(Instant::now());
    flush(socket, &mut turn);
    // The release's answer is not waited for: the allocation expires
    // anyway.
    std::thread::sleep(Duration::from_millis(10));
}
