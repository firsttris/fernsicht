//! Pipeline primitives shared by host and client.
//!
//! Everything in here is used on the hot path, so nothing allocates after
//! construction.

pub mod clock;
pub mod desktop;
pub mod latency;
pub mod slot;
pub mod thread;

pub use clock::now_us;
pub use slot::Slot;
