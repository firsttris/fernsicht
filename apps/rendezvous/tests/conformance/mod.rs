//! What every rendezvous service must do, whichever implementation: the
//! Rust one here, or the Cloudflare Worker (`web/rendezvous-worker`, run
//! with `wrangler dev`, see `tests/worker.rs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fernsicht_internet::rendezvous::{self, Error, HostConnection};
use fernsicht_internet::signal::{
    self, AnswerPayload, ErrorCode, OfferPayload, make_answer, make_offer, open_answer, open_offer,
};
use fernsicht_secure::Identity;
use tungstenite::Message;

pub fn offer_payload(time_ms: u64) -> OfferPayload {
    OfferPayload {
        time_ms,
        candidates: vec![],
    }
}

/// A host that answers every offer it can open, until `stop`; refuses
/// the others. Ends with how many it answered.
pub fn answering_host(
    url: &str,
    host: Identity,
    stop: Arc<AtomicBool>,
) -> (bool, std::thread::JoinHandle<u32>) {
    let mut conn = HostConnection::open(url, &host).unwrap();
    let relay = conn.relay;
    let thread = std::thread::spawn(move || {
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
    });
    (relay, thread)
}

/// Offers and answers, refusals, unknown hosts, replaced registrations
/// and impostors at the service at `url`; `relay`: whether it hands out
/// TURN credentials.
pub fn conforms(url: &str, relay: bool) {
    let host = Identity::generate();
    let host_key = host.public;
    let client = Identity::generate();
    let stop = Arc::new(AtomicBool::new(false));
    let (told_relay, thread) = answering_host(url, host, stop.clone());
    assert_eq!(told_relay, relay);

    // An offer, its answer.
    let (offered, sealed) = make_offer(&client, &host_key, &offer_payload(1)).unwrap();
    let answer = rendezvous::connect(url, &host_key, sealed).unwrap();
    let answer = open_answer(offered, &answer).unwrap();
    assert_eq!(answer.token, "0123");
    assert_eq!(!answer.ice_servers.is_empty(), relay);

    // One the host cannot open is refused at once.
    let (_, sealed) = make_offer(&client, &Identity::generate().public, &offer_payload(2)).unwrap();
    let t0 = std::time::Instant::now();
    match rendezvous::connect(url, &host_key, sealed) {
        Err(Error::Service(ErrorCode::Refused, _)) => {}
        other => panic!("{other:?}"),
    }
    assert!(t0.elapsed() < Duration::from_secs(5));
    stop.store(true, Ordering::Relaxed);
    assert_eq!(thread.join().unwrap(), 1);

    // Nobody registered.
    let nobody = Identity::generate().public;
    let (_, sealed) = make_offer(&client, &nobody, &offer_payload(3)).unwrap();
    match rendezvous::connect(url, &nobody, sealed) {
        Err(Error::Service(ErrorCode::Offline, _)) => {}
        other => panic!("{other:?}"),
    }

    // A second registration replaces the first.
    let again = Identity::generate();
    let mut first = HostConnection::open(url, &again).unwrap();
    let _second = HostConnection::open(url, &again).unwrap();
    let err = (0..60)
        .find_map(|_| first.next_offer(Duration::from_millis(50)).err())
        .expect("the first connection ends");
    assert!(
        matches!(err, Error::Service(ErrorCode::Replaced, _)),
        "{err}"
    );

    impostors_are_refused(url);
}

/// Someone registering a key they do not hold, or one that is not the
/// path's.
fn impostors_are_refused(url: &str) {
    let ws = url.replacen("http://", "ws://", 1);
    let victim = Identity::generate().public;
    let id = signal::rendezvous_id(&victim);
    let (mut sock, _) = tungstenite::connect(format!("{ws}/v1/host/{id}")).unwrap();
    let key = signal::hex(&victim.0);
    sock.send(Message::text(format!(
        r#"{{"type":"register","key":"{key}"}}"#
    )))
    .unwrap();
    assert!(
        sock.read()
            .unwrap()
            .to_text()
            .unwrap()
            .contains("challenge")
    );
    let mac = "00".repeat(32);
    sock.send(Message::text(format!(
        r#"{{"type":"proof","mac":"{mac}"}}"#
    )))
    .unwrap();
    let answer = sock.read().unwrap();
    assert!(
        answer.to_text().unwrap().contains("unauthorized"),
        "{answer:?}"
    );

    let (mut sock, _) = tungstenite::connect(format!("{ws}/v1/host/{id}")).unwrap();
    let other = signal::hex(&Identity::generate().public.0);
    sock.send(Message::text(format!(
        r#"{{"type":"register","key":"{other}"}}"#
    )))
    .unwrap();
    assert!(
        sock.read()
            .unwrap()
            .to_text()
            .unwrap()
            .contains("badRequest")
    );
}
