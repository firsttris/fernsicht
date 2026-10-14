//! The Rust service with real WebSockets: it does what every rendezvous
//! service must (`conformance`), hands out coturn credentials, and limits
//! floods.

mod conformance;

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fernsicht_internet::rendezvous::{self, Error};
use fernsicht_internet::signal::{ErrorCode, OFFERS_PER_MINUTE, make_offer};
use fernsicht_rendezvous::{Config, serve, turn::TurnConfig};
use fernsicht_secure::Identity;

struct Service {
    url: String,
    stop: Arc<AtomicBool>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn start(cfg: Config) -> Service {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    serve(listener, cfg, stop.clone()).unwrap();
    Service { url, stop }
}

#[test]
fn the_service_conforms_without_a_relay() {
    conformance::conforms(&start(Config::default()).url, false);
}

#[test]
fn the_service_conforms_with_coturn_credentials() {
    let turn = TurnConfig {
        urls: vec!["turn:127.0.0.1:3478?transport=udp".into()],
        secret: "s".into(),
        ttl: Duration::from_secs(60),
    };
    conformance::conforms(&start(Config { turn: Some(turn) }).url, true);
}

#[test]
fn floods_are_limited() {
    let svc = start(Config::default());
    let client = Identity::generate();
    let nobody = Identity::generate().public;
    let (_, sealed) = make_offer(&client, &nobody, &conformance::offer_payload(1)).unwrap();
    for _ in 0..OFFERS_PER_MINUTE {
        let _ = rendezvous::connect(&svc.url, &nobody, sealed.clone());
    }
    match rendezvous::connect(&svc.url, &nobody, sealed) {
        Err(Error::Service(ErrorCode::RateLimited, _)) => {}
        other => panic!("{other:?}"),
    }
}
