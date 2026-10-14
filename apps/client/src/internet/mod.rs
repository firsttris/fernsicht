//! Reaching a host over the internet (`docs/internet.md`): through its
//! rendezvous service, hole punched or relayed.

mod connect;
mod relay;

pub use connect::{Options, Reached, answers_directly, over_internet};
pub use relay::Relay;
