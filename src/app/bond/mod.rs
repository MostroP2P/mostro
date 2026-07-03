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

pub mod crud;
pub mod db;
pub mod flow;
pub mod math;
pub mod model;
pub mod payout;
pub mod slash;
pub mod types;

pub use flow::{
    maker_bond_required, release_bond, release_bonds_for_order, release_bonds_for_order_or_warn,
    release_taker_bonds_for_order_or_warn, request_maker_bond, request_taker_bond,
    resubscribe_active_bonds, taker_bond_required, trade_committed_by_locked_taker_bond,
    TakerContext,
};
pub use math::{compute_bond_amount, compute_node_share};
pub use model::Bond;
pub use payout::{add_bond_invoice_action, run_bond_payout_cycle};
pub use slash::{
    apply_bond_resolution, extract_bond_resolution, notify_bond_slashed,
    reconcile_stranded_range_maker_bonds, resolve_range_maker_bond_at_close,
    resolve_range_maker_bond_at_close_or_warn, slash_or_release_on_timeout,
    validate_bond_resolution,
};
pub use types::{BondRole, BondSlashReason, BondState};
