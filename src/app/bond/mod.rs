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
pub mod flow;
pub mod math;
pub mod model;
pub mod slash;
pub mod types;

pub use flow::{
    release_bond, release_bonds_for_order, release_bonds_for_order_or_warn, request_taker_bond,
    resubscribe_active_bonds, taker_bond_required, TakerContext,
};
pub use math::{compute_bond_amount, compute_node_share};
pub use model::Bond;
pub use slash::{apply_bond_resolution, extract_bond_resolution, validate_bond_resolution};
pub use types::{BondRole, BondSlashReason, BondState};
