//! TURN credentials for coturn's shared secret (`use-auth-secret`, the
//! “TURN REST API” scheme): the user name is an expiry time, the password
//! an HMAC of it. coturn checks both without knowing the user.

use std::time::Duration;

use base64::Engine;
use fernsicht_internet::signal::IceServer;
use hmac::{Hmac, Mac};

/// A TURN server the service hands out credentials for.
#[derive(Clone, Debug)]
pub struct TurnConfig {
    /// Its URLs, e.g. `turn:turn.example.org:3478?transport=udp`.
    pub urls: Vec<String>,
    /// coturn's `static-auth-secret`.
    pub secret: String,
    /// How long the credentials are valid.
    pub ttl: Duration,
}

impl TurnConfig {
    /// Credentials valid from `now` (Unix seconds) for the TTL.
    pub fn credentials(&self, now: u64) -> IceServer {
        let username = format!("{}:fernsicht", now + self.ttl.as_secs());
        let mut mac =
            Hmac::<sha1::Sha1>::new_from_slice(self.secret.as_bytes()).expect("HMAC takes any key");
        mac.update(username.as_bytes());
        let credential =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        IceServer {
            urls: self.urls.clone(),
            username: Some(username),
            credential: Some(credential),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_coturns_rest_scheme() {
        let t = TurnConfig {
            urls: vec!["turn:turn.example.org:3478?transport=udp".into()],
            secret: "north".into(),
            ttl: Duration::from_secs(3600),
        };
        let c = t.credentials(1_700_000_000);
        assert_eq!(c.username.as_deref(), Some("1700003600:fernsicht"));
        // base64(HMAC-SHA1("north", "1700003600:fernsicht")), as
        // `printf %s 1700003600:fernsicht | openssl dgst -sha1 -hmac north
        // -binary | base64` prints it.
        assert_eq!(c.credential.as_deref(), Some(EXPECTED));
        assert_eq!(c.urls, t.urls);
    }

    const EXPECTED: &str = "aylINnKQigYDpYrq+69P1NxN27U=";
}
