//! Anti-abuse bond module (issue #711).
//!
//! Phase 0 delivers only the data plumbing: configuration types live in
//! `crate::config::types`, and this module provides the bond model, the
//! `BondRole` / `BondState` / `BondSlashReason` enums, and the pure helper
//! for computing a bond amount. Later phases add the trade-flow hooks; see
//! `docs/ANTI_ABUSE_BOND.md`.
//!
//! Everything here is **inert** unless `Settings::is_bond_enabled()` is true.
//! Callers must gate on that flag.

pub mod db;
pub mod math;
pub mod model;
pub mod types;

pub use math::compute_bond_amount;
pub use model::Bond;
pub use types::{BondRole, BondSlashReason, BondState};
