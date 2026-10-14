//! The service with real WebSockets: a host registers and answers, a
//! client's offer reaches it and the answer comes back; impostors, unknown
//! hosts and floods are turned away.

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fernsicht_internet::rendezvous::{self, Error, HostConnection};
use fernsicht_internet::signal::{
    AnswerPayload, ErrorCode, OFFERS_PER_MINUTE, OfferPayload, make_answer, make_offer,
    open_answer, open_offer,
};
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

fn offer_payload(time_ms: u64) -> OfferPayload {
    OfferPayload {
        time_ms,
        candidates: vec![],
    }
}

/// A host that answers every offer it can open with `token`, until
/// `stop`; refuses the others.
fn answering_host(
    url: &str,
    host: Identity,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<u32> {
    let mut conn = HostConnection::open(url, &host).unwrap();
    std::thread::spawn(move || {
        let mut answered = 0;
        while !stop.load(Ordering::Relaxed) {
            let Ok(Some(offer)) = conn.next_offer(Duration::from_millis(50)) else {
                continue;
            };
            match open_offer(&host, &offer.offer) {
                Ok((opened, _)) => {
                    let answer = make_answer(
                        opened,
                        &AnswerPayload {
                            candidates: vec![],
                            token: "0123".into(),
                            ice_servers: offer.ice_servers,
                        },
                    )
                    .unwrap();
                    conn.answer(offer.connection, answer).unwrap();
                    answered += 1;
                }
                Err(_) => conn.refuse(offer.connection).unwrap(),
            }
        }
        answered
    })
}

#[test]
fn an_offer_reaches_the_host_and_its_answer_the_client() {
    let turn = TurnConfig {
        urls: vec!["turn:127.0.0.1:3478?transport=udp".into()],
        secret: "s".into(),
        ttl: Duration::from_secs(60),
    };
    let svc = start(Config { turn: Some(turn) });
    let host = Identity::generate();
    let host_key = host.public;
    let client = Identity::generate();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = answering_host(&svc.url, host, stop.clone());

    let (offered, sealed) = make_offer(&client, &host_key, &offer_payload(1)).unwrap();
    let answer = rendezvous::connect(&svc.url, &host_key, sealed).unwrap();
    let answer = open_answer(offered, &answer).unwrap();
    assert_eq!(answer.token, "0123");
    assert_eq!(
        answer.ice_servers.len(),
        1,
        "TURN credentials passed through the host"
    );
    assert!(answer.ice_servers[0].username.is_some());

    // An offer the host cannot open (sealed for another key) is refused
    // at once, not left to time out.
    let (_, sealed) = make_offer(&client, &Identity::generate().public, &offer_payload(2)).unwrap();
    let t0 = std::time::Instant::now();
    match rendezvous::connect(&svc.url, &host_key, sealed) {
        Err(Error::Service(ErrorCode::Refused, _)) => {}
        other => panic!("{other:?}"),
    }
    assert!(t0.elapsed() < Duration::from_secs(5));
    stop.store(true, Ordering::Relaxed);
    assert_eq!(thread.join().unwrap(), 1);
}

#[test]
fn unknown_hosts_are_offline_and_floods_are_limited() {
    let svc = start(Config::default());
    let client = Identity::generate();
    let nobody = Identity::generate().public;
    let (_, sealed) = make_offer(&client, &nobody, &offer_payload(1)).unwrap();
    match rendezvous::connect(&svc.url, &nobody, sealed.clone()) {
        Err(Error::Service(ErrorCode::Offline, _)) => {}
        other => panic!("{other:?}"),
    }
    // The first one counted too.
    for _ in 1..OFFERS_PER_MINUTE {
        let _ = rendezvous::connect(&svc.url, &nobody, sealed.clone());
    }
    match rendezvous::connect(&svc.url, &nobody, sealed) {
        Err(Error::Service(ErrorCode::RateLimited, _)) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_second_registration_replaces_the_first() {
    let svc = start(Config::default());
    let host = Identity::generate();
    let mut first = HostConnection::open(&svc.url, &host).unwrap();
    assert!(!first.relay);
    let _second = HostConnection::open(&svc.url, &host).unwrap();
    let err = (0..40)
        .find_map(|_| first.next_offer(Duration::from_millis(50)).err())
        .expect("the first connection ends");
    assert!(
        matches!(err, Error::Service(ErrorCode::Replaced, _)),
        "{err}"
    );
}

#[test]
fn only_the_key_holder_registers() {
    // A raw WebSocket that registers someone else's key and cannot prove it.
    use tungstenite::Message;
    let svc = start(Config::default());
    let victim = Identity::generate().public;
    let id = fernsicht_internet::signal::rendezvous_id(&victim);
    let url = format!("{}/v1/host/{id}", svc.url.replace("http://", "ws://"));
    let (mut ws, _) = tungstenite::connect(url).unwrap();
    let register = format!(
        r#"{{"type":"register","key":"{}"}}"#,
        fernsicht_internet::signal::hex(&victim.0)
    );
    ws.send(Message::text(register)).unwrap();
    let challenge = ws.read().unwrap();
    assert!(challenge.to_text().unwrap().contains("challenge"));
    ws.send(Message::text(format!(
        r#"{{"type":"proof","mac":"{}"}}"#,
        "00".repeat(32)
    )))
    .unwrap();
    let answer = ws.read().unwrap();
    assert!(
        answer.to_text().unwrap().contains("unauthorized"),
        "{answer:?}"
    );
    // And a key that does not belong to the ID in the path is refused.
    let (mut ws, _) = tungstenite::connect(format!(
        "{}/v1/host/{id}",
        svc.url.replace("http://", "ws://")
    ))
    .unwrap();
    let other = fernsicht_internet::signal::hex(&Identity::generate().public.0);
    ws.send(Message::text(format!(
        r#"{{"type":"register","key":"{other}"}}"#
    )))
    .unwrap();
    assert!(ws.read().unwrap().to_text().unwrap().contains("badRequest"));
}
