//! Asking the router to forward the host's port, while the setting is on
//! (`docs/internet.md`, “Port forwarding by the router”): made, renewed
//! at half its lifetime, removed when the setting goes off or the host
//! stops. The forwarded address is offered to clients as a candidate.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fernsicht_internet::portmap::{self, Mapping, Router};

use super::{Forwarding, InternetCell};
use crate::HostSettings;

/// What a mapping is asked for: two hours, renewed after one.
const LIFETIME_S: u32 = 7200;
/// How long a failed attempt waits before the next.
const RETRY_AFTER: Duration = Duration::from_secs(300);

/// The public address the router forwards, for the control loop.
pub(crate) type ForwardedCell = Arc<Mutex<Option<SocketAddr>>>;

/// What to do next.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Map,
    Unmap,
    Wait,
}

/// The next step: `on` (the setting), the mapping and when it was made,
/// when the last attempt failed.
pub(crate) fn step(
    on: bool,
    made: Option<(&Mapping, Instant)>,
    failed: Option<Instant>,
    now: Instant,
) -> Step {
    match (on, made) {
        (false, Some(_)) => Step::Unmap,
        (false, None) => Step::Wait,
        (true, Some((m, at))) if now.duration_since(at) >= m.renew_after() => Step::Map,
        (true, Some(_)) => Step::Wait,
        (true, None) if failed.is_none_or(|t| now.duration_since(t) >= RETRY_AFTER) => Step::Map,
        (true, None) => Step::Wait,
    }
}

fn report(status: &InternetCell, f: Forwarding) {
    if let Ok(mut cell) = status.lock() {
        cell.forwarding = f;
    }
}

/// Starts the thread for the host's `port`; it ends with `stop`.
pub(crate) fn spawn(
    settings: Arc<HostSettings>,
    port: u16,
    status: InternetCell,
    forwarded: ForwardedCell,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("forwarding".into())
        .spawn(move || run(&settings, port, &status, &forwarded, &stop))
}

fn run(
    settings: &HostSettings,
    port: u16,
    status: &InternetCell,
    forwarded: &ForwardedCell,
    stop: &AtomicBool,
) {
    let mut made: Option<(Mapping, Instant, Router)> = None;
    let mut failed: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        let on = settings.port_mapping();
        match step(
            on,
            made.as_ref().map(|(m, at, _)| (m, *at)),
            failed,
            Instant::now(),
        ) {
            Step::Wait => {}
            Step::Unmap => {
                if let Some((m, _, router)) = made.take() {
                    portmap::unmap(&router, &m);
                    log::info!("the router no longer forwards {}", m.external);
                }
                *forwarded.lock().unwrap_or_else(|e| e.into_inner()) = None;
                report(status, Forwarding::default());
            }
            Step::Map => {
                let previous = made.take();
                let result = Router::find()
                    .ok_or_else(|| "no default route, no router".to_owned())
                    .and_then(|router| {
                        portmap::map(
                            &router,
                            port,
                            LIFETIME_S,
                            previous.as_ref().map(|(m, _, _)| m),
                        )
                        .map(|m| (m, router))
                    });
                match result {
                    Ok((m, router)) => {
                        if previous.is_none() {
                            log::info!(
                                "the router forwards {} to port {port} ({:?})",
                                m.external,
                                m.method
                            );
                        }
                        *forwarded.lock().unwrap_or_else(|e| e.into_inner()) = Some(m.external);
                        report(
                            status,
                            Forwarding {
                                on,
                                external: Some(m.external.to_string()),
                                error: None,
                            },
                        );
                        made = Some((m, Instant::now(), router));
                        failed = None;
                    }
                    Err(e) => {
                        log::warn!("port forwarding: {e}");
                        *forwarded.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        report(
                            status,
                            Forwarding {
                                on,
                                external: None,
                                error: Some(e),
                            },
                        );
                        failed = Some(Instant::now());
                    }
                }
            }
        }
        if !on && made.is_none() {
            report(status, Forwarding::default());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if let Some((m, _, router)) = made {
        portmap::unmap(&router, &m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fernsicht_internet::portmap::Method;

    #[test]
    fn mappings_are_made_renewed_retried_and_removed() {
        let t0 = Instant::now();
        let m = Mapping {
            external: "203.0.113.5:47800".parse().unwrap(),
            lifetime: 7200,
            method: Method::NatPmp,
            internal: "192.168.1.20:47800".parse().unwrap(),
        };
        assert_eq!(step(false, None, None, t0), Step::Wait);
        assert_eq!(step(true, None, None, t0), Step::Map);
        assert_eq!(
            step(true, Some((&m, t0)), None, t0 + Duration::from_secs(10)),
            Step::Wait
        );
        assert_eq!(
            step(true, Some((&m, t0)), None, t0 + Duration::from_secs(3600)),
            Step::Map,
            "renewed at half"
        );
        assert_eq!(step(false, Some((&m, t0)), None, t0), Step::Unmap);
        assert_eq!(
            step(true, None, Some(t0), t0 + Duration::from_secs(10)),
            Step::Wait,
            "after a failure"
        );
        assert_eq!(step(true, None, Some(t0), t0 + RETRY_AFTER), Step::Map);
    }
}
