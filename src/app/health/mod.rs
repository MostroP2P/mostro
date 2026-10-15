//! Health monitoring (issue #1063).
//!
//! - [`lnd_sync`] tracks whether the Lightning node is synced to chain and
//!   reachable, defers outgoing payments while it is not, and drives the
//!   automatic (fail-closed) maintenance mode.
//! - [`check`] holds the check states shared with the health event.

pub mod check;
pub mod lnd_sync;
