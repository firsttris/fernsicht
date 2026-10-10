//! Who may connect, and encryption of everything they send.
//!
//! - [`Identity`]: a device's long-term key pair (X25519).
//! - [`Trusted`]: the devices this one is paired with.
//! - [`pairing`]: pairing once with a 6-digit PIN, by SPAKE2: someone who
//!   records the exchange cannot test PINs offline; a wrong guess fails
//!   the pairing (the host gives up after a few).
//! - [`session`]: each connection starts with a Noise IK handshake (as
//!   WireGuard): both prove their paired keys, fresh keys per session.
//!   Then every packet is sealed with ChaCha20-Poly1305 under an explicit
//!   counter; replays are dropped.

pub mod pairing;
pub mod replay;
pub mod session;

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A public key (X25519), as a device is known by others.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PublicKey(#[serde(with = "hex32")] pub [u8; 32]);

impl PublicKey {
    /// A short form to show people (first 8 bytes, grouped).
    pub fn fingerprint(&self) -> String {
        self.0[..8]
            .chunks(2)
            .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
            .collect::<Vec<_>>()
            .join("-")
    }
}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({})", self.fingerprint())
    }
}

mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        if s.len() != 64 {
            return Err(D::Error::custom("expected 64 hex digits"));
        }
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(D::Error::custom)?;
        }
        Ok(out)
    }
}

/// Fills `buf` from the OS's random source.
pub fn random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("the OS random source failed");
}

/// A device's long-term key pair.
#[derive(Clone, Serialize, Deserialize)]
pub struct Identity {
    #[serde(with = "hex32")]
    secret: [u8; 32],
    pub public: PublicKey,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({:?})", self.public)
    }
}

impl Identity {
    pub fn generate() -> Self {
        let kp = snow::Builder::new(session::PARAMS.parse().expect("valid Noise params"))
            .generate_keypair()
            .expect("key generation");
        let mut secret = [0u8; 32];
        let mut public = [0u8; 32];
        secret.copy_from_slice(&kp.private);
        public.copy_from_slice(&kp.public);
        Self {
            secret,
            public: PublicKey(public),
        }
    }

    pub(crate) fn secret(&self) -> &[u8; 32] {
        &self.secret
    }

    /// Loads the identity from `path`, or creates and saves a new one
    /// (readable by the owner only).
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(json) => serde_json::from_str(&json).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = Self::generate();
                write_private(
                    path,
                    &serde_json::to_string_pretty(&id).expect("serializable"),
                )?;
                Ok(id)
            }
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

/// Writes a file only its owner can read (keys, the paired list).
fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(contents.as_bytes())
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// A paired device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    pub key: PublicKey,
    /// Last known address (clients remember where their hosts are).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Unix time of the pairing.
    pub paired_at: u64,
}

/// The devices this one is paired with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trusted {
    pub peers: Vec<Peer>,
}

impl Trusted {
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(json) => serde_json::from_str(&json).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        write_private(
            path,
            &serde_json::to_string_pretty(self).expect("serializable"),
        )
    }

    pub fn get(&self, key: &PublicKey) -> Option<&Peer> {
        self.peers.iter().find(|p| p.key == *key)
    }

    /// Adds `peer`, replacing an entry with the same key.
    pub fn add(&mut self, peer: Peer) {
        self.peers.retain(|p| p.key != peer.key);
        self.peers.push(peer);
    }

    pub fn remove(&mut self, key: &PublicKey) -> bool {
        let before = self.peers.len();
        self.peers.retain(|p| p.key != *key);
        self.peers.len() != before
    }
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("fernsicht-secure-{}-{name}", std::process::id()))
    }

    #[test]
    fn identity_persists_and_is_private() {
        let path = temp("id.json");
        let _ = std::fs::remove_file(&path);
        let a = Identity::load_or_create(&path).unwrap();
        let b = Identity::load_or_create(&path).unwrap();
        assert_eq!(a.public, b.public);
        assert_eq!(a.secret, b.secret);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_file(&path).unwrap();
        assert_ne!(Identity::generate().public, a.public);
        // Debug output shows the public key only.
        assert!(format!("{a:?}").starts_with("Identity(PublicKey("));
    }

    #[test]
    fn trusted_list_round_trips() {
        let path = temp("trusted.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(Trusted::load(&path).unwrap(), Trusted::default());
        let key = Identity::generate().public;
        let mut t = Trusted::default();
        t.add(Peer {
            name: "bazzite".into(),
            key,
            address: None,
            paired_at: 1,
        });
        t.add(Peer {
            name: "bazzite (neu)".into(),
            key,
            address: Some("192.168.178.20:47800".into()),
            paired_at: 2,
        });
        assert_eq!(t.peers.len(), 1, "same key replaces");
        t.save(&path).unwrap();
        let back = Trusted::load(&path).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.get(&key).unwrap().name, "bazzite (neu)");
        let mut back = back;
        assert!(back.remove(&key));
        assert!(!back.remove(&key));
        std::fs::write(&path, "nonsense").unwrap();
        assert!(Trusted::load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn fingerprints_are_short_and_stable() {
        let k = PublicKey([0xab; 32]);
        assert_eq!(k.fingerprint(), "abab-abab-abab-abab");
        let json = serde_json::to_string(&k).unwrap();
        assert_eq!(serde_json::from_str::<PublicKey>(&json).unwrap(), k);
        assert!(serde_json::from_str::<PublicKey>("\"abc\"").is_err());
    }
}
