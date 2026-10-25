//! Health monitoring (issue #1063).
//!
//! - [`lnd_sync`] tracks whether the Lightning node is synced to chain and
//!   reachable, defers outgoing payments while it is not, and drives the
//!   automatic (fail-closed) maintenance mode.
//! - [`check`] holds the checks and the status derivation of the health
//!   event; [`content`] its encrypted payload; [`event`] the tags, the signed
//!   event and the state persisted across restarts; [`stats`] the payout and
//!   dev fee counters; [`cashu_mint`] the mint probe of Cashu instances;
//!   [`publisher`] the job that ties them together.
//!
//! Protocol: `health_event.md` in the protocol book (kind 38389).

pub mod cashu_mint;
pub mod check;
pub mod content;
pub mod event;
pub mod lnd_sync;
pub mod publisher;
pub mod stats;
