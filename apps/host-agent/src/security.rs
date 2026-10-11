//! The host's keys, the paired clients, pairing with a PIN and the
//! handshakes that start secure sessions.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fernsicht_proto::RejectReason;
use fernsicht_secure::pairing::HostPairing;
use fernsicht_secure::{Identity, Peer, PublicKey, Trusted};

/// Where the handshake times live: next to the paired list.
fn handshakes_path(trusted: &Path) -> PathBuf {
    trusted.with_file_name("handshakes.json")
}

/// How long pairing mode stays open, and how many wrong PINs it takes.
pub const PAIRING_OPEN_FOR: Duration = Duration::from_secs(300);
pub const PAIRING_ATTEMPTS: u8 = 3;

/// Attempts a pairing window allows from all addresses together; then it
/// closes. One address alone cannot close it for the others.
pub const PAIRING_ATTEMPTS_TOTAL: u8 = 10;

struct PairingWindow {
    pin: String,
    until: Instant,
    /// Attempts left from all addresses together.
    attempts_left: u8,
    /// Attempts used per address ([`PAIRING_ATTEMPTS`] each).
    used: HashMap<std::net::IpAddr, u8>,
    /// Which opening this is: a device pairing that began in one window
    /// must finish in it (see [`HostSecurity::pairing_generation`]).
    generation: u64,
}

/// Counts pairing windows across the host's life.
static PAIRING_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Who this host is and whom it lets in. Without it (library default, for
/// tests) sessions are not encrypted and anyone may connect.
pub struct HostSecurity {
    pub(crate) identity: Identity,
    pub(crate) name: String,
    trusted: Mutex<Trusted>,
    /// Where the paired list is saved (none: kept in memory, tests).
    trusted_path: Option<PathBuf>,
    pairing: Mutex<Option<PairingWindow>>,
    /// The newest handshake time (the client's clock, ms) per device: an
    /// older or equal one is a replay. Kept next to the paired list, so a
    /// restart does not forget it.
    handshakes: Mutex<HashMap<PublicKey, u64>>,
    /// Counts devices that lost access (unpaired): the running session
    /// checks its client when it changes.
    revocations: AtomicU64,
    /// Someone asked to end the running session (control socket).
    end_requested: AtomicBool,
}

impl std::fmt::Debug for HostSecurity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HostSecurity({}, {:?})", self.name, self.identity.public)
    }
}

impl HostSecurity {
    pub fn new(
        identity: Identity,
        name: &str,
        trusted: Trusted,
        trusted_path: Option<PathBuf>,
    ) -> Self {
        let handshakes = trusted_path
            .as_deref()
            .map(handshakes_path)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Vec<(PublicKey, u64)>>(&t).ok())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();
        Self {
            identity,
            name: name.to_owned(),
            trusted: Mutex::new(trusted),
            trusted_path,
            pairing: Mutex::new(None),
            handshakes: Mutex::new(handshakes),
            revocations: AtomicU64::new(0),
            end_requested: AtomicBool::new(false),
        }
    }

    /// Ends the running session (whoever it is), from the control socket.
    pub fn end_session(&self) {
        self.end_requested.store(true, Ordering::Release);
    }

    pub(crate) fn take_end_request(&self) -> bool {
        self.end_requested.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn revocations(&self) -> u64 {
        self.revocations.load(Ordering::Acquire)
    }

    /// Whether a handshake of `key` sent at `sent_ms` (its clock) is newer
    /// than the last one accepted: older or equal ones are replays.
    pub(crate) fn handshake_is_new(&self, key: &PublicKey, sent_ms: u64) -> bool {
        let times = self.handshakes.lock().unwrap_or_else(|e| e.into_inner());
        times.get(key).is_none_or(|&t| sent_ms > t)
    }

    /// Notes an accepted handshake, also on disk.
    pub(crate) fn note_handshake(&self, key: PublicKey, sent_ms: u64) {
        let mut times = self.handshakes.lock().unwrap_or_else(|e| e.into_inner());
        times.insert(key, sent_ms);
        self.save_handshakes(&times);
    }

    fn save_handshakes(&self, times: &HashMap<PublicKey, u64>) {
        let Some(path) = self.trusted_path.as_deref().map(handshakes_path) else {
            return;
        };
        let list: Vec<(&PublicKey, &u64)> = times.iter().collect();
        let tmp = path.with_extension("json.tmp");
        let saved = serde_json::to_string(&list)
            .map_err(std::io::Error::other)
            .and_then(|t| std::fs::write(&tmp, t))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = saved {
            log::warn!("saving handshake times: {e}");
        }
    }

    /// Key and paired clients from `dir` (`host.json`, `clients.json`),
    /// created on first use.
    pub fn load(dir: &Path, name: &str) -> anyhow::Result<Self> {
        let identity =
            Identity::load_or_create(&dir.join("host.json")).map_err(anyhow::Error::msg)?;
        let path = dir.join("clients.json");
        let trusted = Trusted::load(&path).map_err(anyhow::Error::msg)?;
        Ok(Self::new(identity, name, trusted, Some(path)))
    }

    pub fn public_key(&self) -> PublicKey {
        self.identity.public
    }

    /// Lets one client pair with `pin` within [`PAIRING_OPEN_FOR`].
    pub fn open_pairing(&self, pin: &str) {
        *self.pairing.lock().unwrap_or_else(|e| e.into_inner()) = Some(PairingWindow {
            pin: pin.to_owned(),
            until: Instant::now() + PAIRING_OPEN_FOR,
            attempts_left: PAIRING_ATTEMPTS_TOTAL,
            used: HashMap::new(),
            generation: PAIRING_GENERATION.fetch_add(1, Ordering::Relaxed) + 1,
        });
    }

    /// The pairing window open now (not closed, not expired), if any. A
    /// device pairing finishes only in the window it began in: once a
    /// browser used the PIN (it travels in clear over HTTP) or the window
    /// closed, a pairing someone began with that PIN is refused. Used-up
    /// attempts don't count here: SPAKE2 tells a wrong PIN only at the
    /// end, so the last allowed attempt must still be able to finish.
    pub(crate) fn pairing_generation(&self) -> Option<u64> {
        self.pairing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|p| p.until > Instant::now())
            .map(|p| p.generation)
    }

    pub fn paired(&self) -> Vec<Peer> {
        self.trusted
            .lock()
            .map(|t| t.peers.clone())
            .unwrap_or_default()
    }

    pub(crate) fn is_paired(&self, key: &PublicKey) -> Option<Peer> {
        self.trusted.lock().ok()?.get(key).cloned()
    }

    pub(crate) fn add(&self, peer: Peer) {
        // Paired anew: its clock may have been set back meanwhile.
        {
            let mut times = self.handshakes.lock().unwrap_or_else(|e| e.into_inner());
            if times.remove(&peer.key).is_some() {
                self.save_handshakes(&times);
            }
        }
        let mut t = self.trusted.lock().unwrap_or_else(|e| e.into_inner());
        t.add(peer);
        if let Some(path) = &self.trusted_path
            && let Err(e) = t.save(path)
        {
            log::error!("saving the paired clients: {e}");
        }
    }

    /// The PIN and the window's generation if pairing is open; uses up an
    /// attempt.
    pub(crate) fn pairing_attempt(
        &self,
        from: std::net::IpAddr,
    ) -> Result<(String, u64), RejectReason> {
        let mut w = self.pairing.lock().unwrap_or_else(|e| e.into_inner());
        match w.as_mut() {
            // This address used its attempts: turned away, the window stays
            // open for the others.
            Some(p)
                if p.until > Instant::now()
                    && p.used.get(&from).is_some_and(|&n| n >= PAIRING_ATTEMPTS) =>
            {
                Err(RejectReason::TooManyAttempts)
            }
            Some(p) if p.until > Instant::now() && p.attempts_left > 0 => {
                p.attempts_left -= 1;
                *p.used.entry(from).or_default() += 1;
                Ok((p.pin.clone(), p.generation))
            }
            Some(p) if p.attempts_left == 0 => {
                *w = None;
                Err(RejectReason::TooManyAttempts)
            }
            _ => {
                *w = None;
                Err(RejectReason::PairingClosed)
            }
        }
    }

    pub fn close_pairing(&self) {
        *self.pairing.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// How long pairing stays open, if it is.
    pub fn pairing_remaining(&self) -> Option<Duration> {
        let w = self.pairing.lock().unwrap_or_else(|e| e.into_inner());
        w.as_ref()
            .filter(|p| p.attempts_left > 0)
            .and_then(|p| p.until.checked_duration_since(Instant::now()))
    }

    /// Forgets a paired device, by name or key fingerprint.
    pub fn unpair(&self, device: &str) -> Result<(), &'static str> {
        let mut t = self.trusted.lock().unwrap_or_else(|e| e.into_inner());
        // The key, or a name only one device has: names are chosen by the
        // devices themselves.
        let by_key = t.peers.iter().find(|p| p.key.fingerprint() == device);
        let mut named = t.peers.iter().filter(|p| p.name == device);
        let key = match (by_key, named.next(), named.next()) {
            (Some(p), _, _) | (None, Some(p), None) => p.key,
            (None, Some(_), Some(_)) => return Err("several devices have that name; use the key"),
            (None, None, _) => return Err("no such paired device"),
        };
        t.remove(&key);
        if let Some(path) = &self.trusted_path
            && let Err(e) = t.save(path)
        {
            log::error!("saving the paired clients: {e}");
        }
        // A session of that device ends with it.
        self.revocations.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

/// A pairing in progress with one client (retransmissions get the same
/// answers).
pub(crate) struct PairingExchange {
    /// The pairing window it began in.
    pub(crate) generation: u64,
    pub(crate) from: SocketAddr,
    pub(crate) msg1: Vec<u8>,
    pub(crate) msg2: Vec<u8>,
    pub(crate) state: Option<HostPairing>,
    /// Message 3 and our answer, once paired.
    pub(crate) done: Option<(Vec<u8>, Vec<u8>)>,
}
