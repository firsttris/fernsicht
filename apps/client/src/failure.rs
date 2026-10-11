//! Why pairing or a session failed, with a code: the app tells failures
//! apart by it and says them in its own words, instead of reading ours.

use std::fmt;

use fernsicht_proto::RejectReason;
use serde::{Deserialize, Serialize};

use crate::hosts::reject_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum FailureCode {
    /// Pairing: the PIN does not match.
    WrongPin,
    /// Pairing: too many wrong PINs, the host closed pairing.
    TooManyAttempts,
    /// Pairing: the host shows no PIN.
    PairingClosed,
    /// The host does not know this device (any more).
    NotPaired,
    /// The host could not start the session (no picture, no encoder).
    Unavailable,
    /// Pairing: no answer from the host.
    NoAnswer,
    /// The session's host went quiet.
    HostSilent,
    /// The password prompt was dismissed (sharing this computer).
    Cancelled,
    /// No pkexec to set up the host as administrator.
    PkexecMissing,
    /// Anything else: the message says what.
    Other,
}

/// A failure: its code and the whole message (with where it happened).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Failure {
    pub code: FailureCode,
    pub message: String,
}

impl Failure {
    pub fn new(code: FailureCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The host said no.
    pub fn rejected(r: RejectReason) -> Self {
        let code = match r {
            RejectReason::NotPaired => FailureCode::NotPaired,
            RejectReason::PairingClosed => FailureCode::PairingClosed,
            RejectReason::TooManyAttempts => FailureCode::TooManyAttempts,
            RejectReason::Unavailable => FailureCode::Unavailable,
        };
        Self::new(code, reject_text(r))
    }

    /// The same failure, said of `place` (a host's address).
    pub fn at(self, place: &str) -> Self {
        Self::new(self.code, format!("{place}: {}", self.message))
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

/// The failure `e` carries somewhere in its chain, with `e`'s whole
/// message; `Other` if it carries none.
pub fn failure_of(e: &anyhow::Error) -> Failure {
    let code = e
        .chain()
        .find_map(|c| c.downcast_ref::<Failure>())
        .map_or(FailureCode::Other, |f| f.code);
    Failure::new(code, format!("{e:#}"))
}
