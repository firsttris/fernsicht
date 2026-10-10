//! End-to-end test harness.
//!
//! [`ImpairedLink`] is a UDP proxy between client and host that injects
//! the faults real networks have: random loss, duplication, reordering,
//! delay with jitter, and blackouts. [`Scenario`] runs a real host agent and
//! a real client (in-process, same code as the binaries) through it.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fernsicht_client::{ClientConfig, RunSummary};
use fernsicht_host_agent::{HostAgent, HostConfig, HostStats};
use fernsicht_net::socket::bind_udp;
use fernsicht_proto::Packet;

/// Faults applied to one direction of the link.
#[derive(Clone, Copy, Debug, Default)]
pub struct Impairment {
    /// Probability of dropping a datagram.
    pub loss: f64,
    /// Probability of delivering a datagram twice.
    pub duplicate: f64,
    /// Probability of holding a datagram back so later ones overtake it.
    pub reorder: f64,
    /// Fixed one-way delay.
    pub delay: Duration,
    /// Extra random delay, uniform in `0..=jitter`.
    pub jitter: Duration,
    /// Drop everything from `start` (after the first datagram) for `length`.
    pub blackout: Option<(Duration, Duration)>,
}

impl Impairment {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn loss(p: f64) -> Self {
        Self {
            loss: p,
            ..Self::default()
        }
    }

    fn is_transparent(&self) -> bool {
        self.loss == 0.0
            && self.duplicate == 0.0
            && self.reorder == 0.0
            && self.delay.is_zero()
            && self.jitter.is_zero()
            && self.blackout.is_none()
    }
}

/// Counters for one direction.
#[derive(Debug, Default)]
pub struct DirectionStats {
    pub forwarded: AtomicU64,
    pub dropped: AtomicU64,
    pub duplicated: AtomicU64,
    pub reordered: AtomicU64,
}

/// What the link observed.
#[derive(Debug, Default)]
pub struct LinkStats {
    /// Host → client.
    pub down: DirectionStats,
    /// Client → host.
    pub up: DirectionStats,
    /// Highest FEC redundancy (recovery/data, in ‰) seen on video groups
    /// with at least 10 data shards.
    pub max_redundancy_permille: AtomicU64,
    /// Video keyframe shards seen (first shard of each keyframe).
    pub keyframes: AtomicU64,
}

/// xorshift64*, deterministic per seed.
struct Rng(u64);

impl Rng {
    fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && self.unit() < p
    }

    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Pending {
    due: Instant,
    seq: u64,
    to_client: bool,
    data: Vec<u8>,
}

#[derive(Default)]
struct Queue {
    heap: Mutex<BinaryHeap<Reverse<Pending>>>,
    ready: Condvar,
}

/// A UDP proxy with independent impairments per direction.
pub struct ImpairedLink {
    addr: SocketAddr,
    stats: Arc<LinkStats>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl ImpairedLink {
    /// Proxies to `host`. Clients connect to [`addr`](Self::addr).
    pub fn start(host: SocketAddr, down: Impairment, up: Impairment, seed: u64) -> Self {
        // Large buffers like the real endpoints, so the proxy itself never
        // drops keyframe bursts.
        let client_side = bind_udp("127.0.0.1:0").unwrap();
        let host_side = bind_udp("127.0.0.1:0").unwrap();
        host_side.connect(host).unwrap();
        for s in [&client_side, &host_side] {
            s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        }
        let addr = client_side.local_addr().unwrap();
        let client_side = Arc::new(client_side);
        let host_side = Arc::new(host_side);
        let client_addr = Arc::new(Mutex::new(None::<SocketAddr>));
        let stats = Arc::new(LinkStats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(Queue::default());
        let epoch = Arc::new(Mutex::new(None::<Instant>));
        let seq = Arc::new(AtomicU64::new(0));

        let mut threads = Vec::new();
        // client → host
        {
            let (src, dst) = (client_side.clone(), host_side.clone());
            let (stats, stop, queue, client_addr, epoch, seq) = (
                stats.clone(),
                stop.clone(),
                queue.clone(),
                client_addr.clone(),
                epoch.clone(),
                seq.clone(),
            );
            threads.push(std::thread::spawn(move || {
                let mut rng = Rng(seed | 1);
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    let Ok((n, from)) = src.recv_from(&mut buf) else {
                        continue;
                    };
                    *client_addr.lock().unwrap() = Some(from);
                    let t0 = *epoch.lock().unwrap().get_or_insert_with(Instant::now);
                    forward(
                        &buf[..n],
                        false,
                        &up,
                        &mut rng,
                        t0,
                        &stats.up,
                        &queue,
                        &seq,
                        &|d: &[u8]| {
                            let _ = dst.send(d);
                        },
                    );
                }
            }));
        }
        // host → client
        {
            let (src, dst) = (host_side.clone(), client_side.clone());
            let (stats, stop, queue, client_addr, epoch, seq) = (
                stats.clone(),
                stop.clone(),
                queue.clone(),
                client_addr.clone(),
                epoch.clone(),
                seq.clone(),
            );
            threads.push(std::thread::spawn(move || {
                let mut rng = Rng(seed.rotate_left(17) | 1);
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    let Ok(n) = src.recv(&mut buf) else {
                        continue;
                    };
                    observe(&buf[..n], &stats);
                    let Some(to) = *client_addr.lock().unwrap() else {
                        continue;
                    };
                    let t0 = *epoch.lock().unwrap().get_or_insert_with(Instant::now);
                    forward(
                        &buf[..n],
                        true,
                        &down,
                        &mut rng,
                        t0,
                        &stats.down,
                        &queue,
                        &seq,
                        &|d: &[u8]| {
                            let _ = dst.send_to(d, to);
                        },
                    );
                }
            }));
        }
        // delayed delivery
        {
            let (queue, stop) = (queue.clone(), stop.clone());
            let (client_side, host_side, client_addr) =
                (client_side.clone(), host_side.clone(), client_addr.clone());
            threads.push(std::thread::spawn(move || {
                loop {
                    let mut heap = queue.heap.lock().unwrap();
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let now = Instant::now();
                    match heap.peek() {
                        Some(Reverse(p)) if p.due <= now => {
                            let Reverse(p) = heap.pop().unwrap();
                            drop(heap);
                            if p.to_client {
                                if let Some(to) = *client_addr.lock().unwrap() {
                                    let _ = client_side.send_to(&p.data, to);
                                }
                            } else {
                                let _ = host_side.send(&p.data);
                            }
                        }
                        Some(Reverse(p)) => {
                            let wait = p.due - now;
                            let _ = queue.ready.wait_timeout(heap, wait).unwrap();
                        }
                        None => {
                            let _ = queue
                                .ready
                                .wait_timeout(heap, Duration::from_millis(20))
                                .unwrap();
                        }
                    }
                }
            }));
        }

        Self {
            addr,
            stats,
            stop,
            threads,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stats(&self) -> &LinkStats {
        &self.stats
    }
}

impl Drop for ImpairedLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Records protocol facts the tests assert on (FEC redundancy, keyframes).
fn observe(datagram: &[u8], stats: &LinkStats) {
    if let Ok(Packet::Video(h, _)) = Packet::decode(datagram) {
        if h.data_shards >= 10 {
            let permille = u64::from(h.recovery_shards) * 1000 / u64::from(h.data_shards);
            stats
                .max_redundancy_permille
                .fetch_max(permille, Ordering::Relaxed);
        }
        if h.keyframe && h.group_index == 0 && h.shard_index == 0 {
            stats.keyframes.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn forward(
    data: &[u8],
    to_client: bool,
    imp: &Impairment,
    rng: &mut Rng,
    t0: Instant,
    stats: &DirectionStats,
    queue: &Queue,
    seq: &AtomicU64,
    send_now: &dyn Fn(&[u8]),
) {
    if let Some((start, len)) = imp.blackout {
        let t = t0.elapsed();
        if t >= start && t < start + len {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    if rng.chance(imp.loss) {
        stats.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let copies = if rng.chance(imp.duplicate) {
        stats.duplicated.fetch_add(1, Ordering::Relaxed);
        2
    } else {
        1
    };
    for _ in 0..copies {
        stats.forwarded.fetch_add(1, Ordering::Relaxed);
        if imp.is_transparent() {
            send_now(data);
            continue;
        }
        let mut delay = imp.delay + imp.jitter.mul_f64(rng.unit());
        if rng.chance(imp.reorder) {
            stats.reordered.fetch_add(1, Ordering::Relaxed);
            delay += Duration::from_millis(3);
        }
        let mut heap = queue.heap.lock().unwrap();
        heap.push(Reverse(Pending {
            due: Instant::now() + delay,
            seq: seq.fetch_add(1, Ordering::Relaxed),
            to_client,
            data: data.to_vec(),
        }));
        drop(heap);
        queue.ready.notify_one();
    }
}

/// A running host agent on an ephemeral localhost port.
pub struct Host {
    addr: SocketAddr,
    web_addr: Option<SocketAddr>,
    stats: Arc<HostStats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Host {
    pub fn start(cfg: HostConfig) -> Self {
        let agent = HostAgent::bind(HostConfig {
            bind: "127.0.0.1:0".into(),
            ..cfg
        })
        .expect("bind host");
        let addr = agent.local_addr().unwrap();
        let web_addr = agent.web_addr();
        let stats = agent.stats();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let thread = std::thread::spawn(move || agent.run(s).expect("host failed"));
        Self {
            addr,
            web_addr,
            stats,
            stop,
            thread: Some(thread),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stats(&self) -> Arc<HostStats> {
        self.stats.clone()
    }

    /// The web viewer's address (with `HostConfig::web`).
    pub fn web_addr(&self) -> Option<SocketAddr> {
        self.web_addr
    }

    /// Stops the host (it says `Bye` to its client) and waits for it.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.join().expect("host thread panicked");
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One streaming run: host, impaired link, client.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_kbps: u32,
    pub duration: Duration,
    pub down: Impairment,
    pub up: Impairment,
    pub seed: u64,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_kbps: 10_000,
            duration: Duration::from_secs(3),
            down: Impairment::none(),
            up: Impairment::none(),
            seed: 0x5EED,
        }
    }
}

pub struct Outcome {
    pub summary: RunSummary,
    pub link: Arc<LinkStats>,
    pub host: Arc<HostStats>,
}

impl Outcome {
    /// Frames the host dropped itself because it fell behind (CPU overload
    /// on a busy test machine). These are not network losses.
    pub fn host_overflows(&self) -> u64 {
        HostStats::get(&self.host.send_overflows)
    }

    /// Frames lost that the host did not drop itself: the network's share.
    pub fn network_frame_drops(&self) -> u64 {
        u64::from(self.summary.receiver.frames_dropped).saturating_sub(self.host_overflows())
    }

    /// Frames a perfect run would have shown.
    pub fn expected_frames(scenario: &Scenario) -> u64 {
        (scenario.duration.as_secs_f64() * f64::from(scenario.fps)) as u64
    }
}

/// Streaming runs measure timing, so they must not compete for the CPU.
/// Every run in this process takes this lock (see also [`exclusive`]).
static STREAMING: Mutex<()> = Mutex::new(());

/// Holds the streaming lock for tests that drive host and client directly.
pub fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    STREAMING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Scenario {
    pub fn run(&self) -> Outcome {
        let _serial = exclusive();
        let host = Host::start(HostConfig::default());
        let link = ImpairedLink::start(host.addr(), self.down, self.up, self.seed);
        let summary = fernsicht_client::run(
            ClientConfig {
                host: link.addr().to_string(),
                width: self.width,
                height: self.height,
                fps: self.fps,
                bitrate_kbps: self.bitrate_kbps,
                duration: Some(self.duration),
                ..ClientConfig::default()
            },
            Arc::new(AtomicBool::new(false)),
        )
        .expect("client run failed");
        let stats = link.stats.clone();
        let host_stats = host.stats();
        drop(link);
        host.stop();
        Outcome {
            summary,
            link: stats,
            host: host_stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    fn echo_server() -> (SocketAddr, Arc<AtomicBool>) {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let addr = sock.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while !s.load(Ordering::Relaxed) {
                if let Ok((n, from)) = sock.recv_from(&mut buf) {
                    let _ = sock.send_to(&buf[..n], from);
                }
            }
        });
        (addr, stop)
    }

    fn roundtrips(link: &ImpairedLink, n: u32) -> (u32, Duration) {
        let c = UdpSocket::bind("127.0.0.1:0").unwrap();
        c.connect(link.addr()).unwrap();
        c.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut got = 0;
        let start = Instant::now();
        for i in 0..n {
            c.send(&i.to_le_bytes()).unwrap();
        }
        let mut buf = [0u8; 16];
        while c.recv(&mut buf).is_ok() {
            got += 1;
        }
        (got, start.elapsed())
    }

    #[test]
    fn transparent_link_forwards_everything() {
        let (echo, stop) = echo_server();
        let link = ImpairedLink::start(echo, Impairment::none(), Impairment::none(), 1);
        let (got, _) = roundtrips(&link, 100);
        assert_eq!(got, 100);
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn loss_and_duplication_change_counts() {
        let (echo, stop) = echo_server();
        let down = Impairment {
            loss: 0.3,
            ..Impairment::none()
        };
        let up = Impairment {
            duplicate: 1.0,
            ..Impairment::none()
        };
        let link = ImpairedLink::start(echo, down, up, 7);
        let (got, _) = roundtrips(&link, 200);
        // 200 sent, all duplicated upstream (400), ~30 % lost downstream.
        let st = link.stats();
        assert_eq!(st.up.duplicated.load(Ordering::Relaxed), 200);
        assert_eq!(st.up.forwarded.load(Ordering::Relaxed), 400);
        let dropped = st.down.dropped.load(Ordering::Relaxed) as f64;
        let forwarded = st.down.forwarded.load(Ordering::Relaxed) as f64;
        let ratio = dropped / (dropped + forwarded);
        assert!((0.2..0.4).contains(&ratio), "loss ratio {ratio}");
        assert!(got as f64 <= forwarded, "{got} > {forwarded}");
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn delay_is_applied() {
        let (echo, stop) = echo_server();
        let d = Impairment {
            delay: Duration::from_millis(30),
            ..Impairment::none()
        };
        let link = ImpairedLink::start(echo, d, d, 3);
        let c = UdpSocket::bind("127.0.0.1:0").unwrap();
        c.connect(link.addr()).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let start = Instant::now();
        c.send(b"x").unwrap();
        let mut buf = [0u8; 4];
        c.recv(&mut buf).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(60));
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn blackout_drops_a_window() {
        let (echo, stop) = echo_server();
        let b = Impairment {
            blackout: Some((Duration::ZERO, Duration::from_secs(10))),
            ..Impairment::none()
        };
        let link = ImpairedLink::start(echo, b, Impairment::none(), 3);
        let (got, _) = roundtrips(&link, 20);
        assert_eq!(got, 0);
        stop.store(true, Ordering::Relaxed);
    }
}
