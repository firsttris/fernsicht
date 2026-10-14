//! The Cloudflare Worker (`web/rendezvous-worker`) does what every
//! rendezvous service must. Runs against a Worker that is up already:
//!
//! ```sh
//! cd web/rendezvous-worker && npx wrangler@4 dev --port 8787 &
//! FERNSICHT_TEST_WORKER_URL=http://127.0.0.1:8787 cargo test -p fernsicht-rendezvous --test worker
//! ```
//!
//! Without the variable it passes without checking anything (CI has no
//! workerd; the Worker's logic has its own tests in TypeScript).

mod conformance;

#[test]
fn the_worker_conforms() {
    let Ok(url) = std::env::var("FERNSICHT_TEST_WORKER_URL") else {
        eprintln!("FERNSICHT_TEST_WORKER_URL not set: the Worker is not checked");
        return;
    };
    conformance::conforms(&url, false);
}
