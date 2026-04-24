//! `Bond` is the database row type for the `bonds` table.
//!
//! String-typed `role` / `state` / `slashed_reason` keep the SQL dump
//! readable. The daemon translates through [`super::types`] when it needs
//! to pattern-match.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use sqlx_crud::SqlxCrud;
use uuid::Uuid;

use super::types::{BondRole, BondState};

/// Database representation of an anti-abuse bond row.
///
/// Created only when `[anti_abuse_bond]` is `enabled = true` and the flow
/// in question matches `apply_to`. A bond row can outlive the trade it was
/// attached to, because a slashed bond still needs a payout to complete;
/// that's why fields that only become meaningful after slash (e.g.
/// `payout_invoice`) are optional.
#[derive(Debug, Default, Clone, Deserialize, Serialize, FromRow, SqlxCrud, PartialEq, Eq)]
#[external_id]
pub struct Bond {
    /// Unique identifier for the bond row.
    pub id: Uuid,
    /// Order the bond is attached to.
    pub order_id: Uuid,
    /// For Phase 6 child-slash rows: the parent maker bond. `None` on a
    /// parent row or on a non-range bond.
    pub parent_bond_id: Option<Uuid>,
    /// For Phase 6: the child (range-taken) order id this row represents.
    /// `None` on a parent row or on a non-range bond.
    pub child_order_id: Option<Uuid>,
    /// Trade pubkey of the bonded party. Hex-encoded, 64 chars.
    pub pubkey: String,
    /// `maker` or `taker`. See [`BondRole`].
    pub role: String,
    /// Amount (sats) this bond row represents.
    pub amount_sats: i64,
    /// Running total of sats already slashed from a parent bond; used by
    /// Phase 6 range-order accounting. 0 for child and non-range rows.
    pub slashed_share_sats: i64,
    /// Serialized [`BondState`].
    pub state: String,
    /// Serialized [`super::types::BondSlashReason`]; `None` unless slashed
    /// / pending payout.
    pub slashed_reason: Option<String>,
    /// Bond hold invoice payment hash (hex, 64 chars).
    pub hash: Option<String>,
    /// Preimage retained by Mostro. `None` on child rows that share the
    /// parent HTLC.
    ///
    /// **Secret.** Never serialize: the preimage is the capability that
    /// settles the bond HTLC, so leaking it to an audit event, Nostr
    /// payload, or RPC response would let a third party race Mostro to
    /// claim the bond. `skip_serializing` keeps it out of any serde
    /// output a later phase might accidentally introduce; the field is
    /// still loaded from the DB via `sqlx::FromRow` as normal.
    #[serde(skip_serializing)]
    pub preimage: Option<String>,
    /// bolt11 payment request shown to the bonded party.
    pub payment_request: Option<String>,
    /// bolt11 invoice from the winning counterparty (Phase 3+).
    ///
    /// Defense-in-depth: not a capability like `preimage`, but it
    /// identifies the winner's node and is payable by anyone who sees
    /// it. Kept out of serde output until a phase has a concrete reason
    /// to publish it.
    #[serde(skip_serializing)]
    pub payout_invoice: Option<String>,
    /// Routing-fee ceiling actually used for the payout attempt (sats).
    pub payout_routing_fee_sats: Option<i64>,
    /// Number of payout invoice requests attempted so far.
    pub payout_attempts: i64,
    /// Timestamp when the bond hold invoice reached `Accepted`.
    pub locked_at: Option<i64>,
    /// Timestamp when the bond transitioned to `Released`.
    pub released_at: Option<i64>,
    /// Timestamp when the bond transitioned to `Slashed`.
    pub slashed_at: Option<i64>,
    /// Timestamp when the row was created.
    pub created_at: i64,
}

impl Bond {
    /// Construct a new `Requested` bond row. The caller is responsible for
    /// inserting it via `Crud::create`.
    pub fn new_requested(order_id: Uuid, pubkey: String, role: BondRole, amount_sats: i64) -> Self {
        Self {
            id: Uuid::new_v4(),
            order_id,
            parent_bond_id: None,
            child_order_id: None,
            pubkey,
            role: role.to_string(),
            amount_sats,
            slashed_share_sats: 0,
            state: BondState::Requested.to_string(),
            slashed_reason: None,
            hash: None,
            preimage: None,
            payment_request: None,
            payout_invoice: None,
            payout_routing_fee_sats: None,
            payout_attempts: 0,
            locked_at: None,
            released_at: None,
            slashed_at: None,
            created_at: Utc::now().timestamp(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_requested_defaults() {
        let order_id = Uuid::new_v4();
        let b = Bond::new_requested(order_id, "a".repeat(64), BondRole::Taker, 1_000);
        assert_eq!(b.order_id, order_id);
        assert_eq!(b.role, "taker");
        assert_eq!(b.state, "requested");
        assert_eq!(b.amount_sats, 1_000);
        assert_eq!(b.slashed_share_sats, 0);
        assert!(b.hash.is_none());
        assert!(b.locked_at.is_none());
        assert!(b.released_at.is_none());
        assert!(b.slashed_at.is_none());
    }

    #[test]
    fn serialize_omits_secret_fields() {
        // The preimage is the capability that settles the bond HTLC;
        // `payout_invoice` identifies the winner. Both must stay out of
        // any serde output a future phase accidentally adds.
        let mut b = Bond::new_requested(Uuid::new_v4(), "a".repeat(64), BondRole::Taker, 1_000);
        b.preimage = Some("deadbeef".repeat(8));
        b.payout_invoice = Some("lnbc1pSECRET".to_string());
        b.hash = Some("c0ffee".repeat(10) + "c0ff");

        let json = serde_json::to_string(&b).expect("serialize");
        assert!(!json.contains("preimage"), "preimage leaked: {json}");
        assert!(!json.contains("deadbeef"), "preimage value leaked: {json}");
        assert!(
            !json.contains("payout_invoice"),
            "payout_invoice leaked: {json}"
        );
        assert!(!json.contains("lnbc1pSECRET"), "payout_invoice value leaked: {json}");
        // Non-secret fields still serialize as usual.
        assert!(json.contains("hash"));
        assert!(json.contains("order_id"));
    }
}
