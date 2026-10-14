//! Connections over the internet (`docs/internet.md`).
//!
//! - [`stun`]: STUN and TURN messages, ChannelData, telling them apart
//!   from Fernsicht's packets on one socket.
//! - [`candidates`]: the addresses a device may be reached at, and how
//!   good a path to each is.
//! - [`punch`]: when to send hole punching probes and which path to take.
//! - [`signal`]: the rendezvous service's messages, the host's key proof,
//!   and the sealed offer and answer.
//! - [`rendezvous`]: talking to a rendezvous service over WebSockets (a
//!   host's standing connection, a client's request).
//! - [`turn`]: a TURN client (allocation, channels, ChannelData) as a
//!   state machine.

pub mod candidates;
pub mod punch;
pub mod rendezvous;
pub mod signal;
pub mod stun;
pub mod turn;
