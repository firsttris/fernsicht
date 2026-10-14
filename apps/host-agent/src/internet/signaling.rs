//! The host's standing connection to its rendezvous service: registers
//! (proving its key), passes offers to the control loop and its answers
//! back, and connects again when the connection drops or the address
//! changes.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};
use fernsicht_internet::rendezvous::{HostConnection, IncomingOffer};
use fernsicht_secure::Identity;

use super::{InternetCell, InternetState, InternetStatus};
use crate::HostSettings;

/// An offer for the control loop, with the STUN servers' addresses (looked
/// up here: the control loop must not wait for DNS).
#[derive(Debug)]
pub(crate) struct Offer {
    pub(crate) incoming: IncomingOffer,
    pub(crate) stun: Vec<SocketAddr>,
}

/// The control loop's reply to an offer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    Answer { connection: u32, answer: String },
    Refuse { connection: u32 },
}

pub(crate) struct Signaling {
    pub(crate) offers: Receiver<Offer>,
    pub(crate) replies: Sender<Reply>,
    thread: JoinHandle<()>,
}

impl Signaling {
    pub(crate) fn join(self) {
        let _ = self.thread.join();
    }
}

/// How long a failed connection waits before the next try: doubling from
/// a second to a minute.
pub(crate) fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.min(6)).min(Duration::from_secs(60))
}

/// How often the STUN servers' addresses are looked up again.
const STUN_LOOKUP_EVERY: Duration = Duration::from_secs(600);

/// Starts the thread; it ends with `stop`.
pub(crate) fn spawn(
    settings: Arc<HostSettings>,
    identity: Identity,
    stun: Vec<String>,
    status: InternetCell,
    stop: Arc<AtomicBool>,
) -> std::io::Result<Signaling> {
    let (offers_tx, offers) = bounded(8);
    let (replies, replies_rx) = bounded(8);
    let thread = std::thread::Builder::new()
        .name("rendezvous".into())
        .spawn(move || {
            run(
                &settings,
                &identity,
                &stun,
                &status,
                &stop,
                (&offers_tx, &replies_rx),
            )
        })?;
    Ok(Signaling {
        offers,
        replies,
        thread,
    })
}

fn set(status: &InternetCell, s: InternetStatus) {
    if let Ok(mut cell) = status.lock() {
        *cell = s;
    }
}

/// Sleeps up to `d`, ending early when `stop` or the address changes.
fn wait(d: Duration, settings: &HostSettings, url: &Option<String>, stop: &AtomicBool) {
    let until = Instant::now() + d;
    while Instant::now() < until && !stop.load(Ordering::Relaxed) && settings.rendezvous() == *url {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn run(
    settings: &HostSettings,
    identity: &Identity,
    stun_servers: &[String],
    status: &InternetCell,
    stop: &AtomicBool,
    (offers, replies): (&Sender<Offer>, &Receiver<Reply>),
) {
    let mut failures = 0;
    let mut stun: (Vec<SocketAddr>, Option<Instant>) = (Vec::new(), None);
    while !stop.load(Ordering::Relaxed) {
        let url = settings.rendezvous();
        let Some(base) = url.clone() else {
            set(status, InternetStatus::default());
            wait(Duration::from_secs(1), settings, &url, stop);
            continue;
        };
        set(
            status,
            InternetStatus {
                url: url.clone(),
                state: InternetState::Connecting,
                ..Default::default()
            },
        );
        let mut conn = match HostConnection::open(&base, identity) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("{e}; trying again in {:?}", backoff(failures));
                set(
                    status,
                    InternetStatus {
                        url: url.clone(),
                        state: InternetState::Failed,
                        relay: false,
                        error: Some(e.to_string()),
                    },
                );
                wait(backoff(failures), settings, &url, stop);
                failures += 1;
                continue;
            }
        };
        failures = 0;
        log::info!(
            "registered at the rendezvous service {base}{}",
            if conn.relay { " (with a relay)" } else { "" }
        );
        set(
            status,
            InternetStatus {
                url: url.clone(),
                state: InternetState::Registered,
                relay: conn.relay,
                error: None,
            },
        );
        let lost = serve(
            &mut conn,
            (settings, &url, stop),
            stun_servers,
            &mut stun,
            (offers, replies),
        );
        match lost {
            Some(e) => {
                log::warn!("rendezvous service: {e}; connecting again");
                set(
                    status,
                    InternetStatus {
                        url: url.clone(),
                        state: InternetState::Failed,
                        relay: false,
                        error: Some(e),
                    },
                );
                wait(backoff(0), settings, &url, stop);
            }
            None => conn.close(),
        }
    }
}

/// Passes offers and replies until the connection drops (why) or the
/// address changes or the host stops (`None`).
fn serve(
    conn: &mut HostConnection,
    (settings, url, stop): (&HostSettings, &Option<String>, &AtomicBool),
    stun_servers: &[String],
    stun: &mut (Vec<SocketAddr>, Option<Instant>),
    (offers, replies): (&Sender<Offer>, &Receiver<Reply>),
) -> Option<String> {
    while !stop.load(Ordering::Relaxed) && settings.rendezvous() == *url {
        while let Ok(r) = replies.try_recv() {
            let sent = match r {
                Reply::Answer { connection, answer } => conn.answer(connection, answer),
                Reply::Refuse { connection } => conn.refuse(connection),
            };
            if let Err(e) = sent {
                return Some(e.to_string());
            }
        }
        match conn.next_offer(Duration::from_millis(100)) {
            Ok(None) => {}
            Ok(Some(incoming)) => {
                if stun.1.is_none_or(|t| t.elapsed() >= STUN_LOOKUP_EVERY) || stun.0.is_empty() {
                    *stun = (
                        fernsicht_internet::stun::resolve(stun_servers),
                        Some(Instant::now()),
                    );
                }
                // Full: the control loop is behind; the client tries again.
                let _ = offers.try_send(Offer {
                    incoming,
                    stun: stun.0.clone(),
                });
            }
            Err(e) => return Some(e.to_string()),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_a_minute() {
        let waits: Vec<u64> = (0..9).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }
}
