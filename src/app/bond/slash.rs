//! Phase 2 — solver-directed dispute slash.
//!
//! Translates a [`BondResolution`] payload carried by `Action::AdminSettle`
//! / `Action::AdminCancel` into concrete bond transitions:
//!
//! - **Slashed sides** move from `Locked` → `PendingPayout`, with
//!   `slashed_reason`, `slashed_at`, and `node_share_sats` snapshotted in
//!   the same `UPDATE` so a later config change or daemon restart cannot
//!   rebalance the split.
//! - **Non-slashed sides** are released exactly as Phase 1 did
//!   (`cancel_hold_invoice` + state = `Released`).
//!
//! The actual Lightning payout to the counterparty is the job of Phase 3
//! (`job_process_bond_payouts`). This module never touches the
//! `payout_invoice` / `send_payment` machinery.
//!
//! ## Flow contract
//!
//! Handlers (`admin_settle_action`, `admin_cancel_action`) call
//! [`validate_bond_resolution`] **before** any trade-side mutation. If the
//! solver requested `slash_*=true` for a side with no `Locked` bond row,
//! the validator returns `MostroCantDo(InvalidPayload)`; the handler
//! propagates that and the trade resolution does not run — the solver
//! resends a corrected payload. After the trade-side mutation succeeds,
//! the handler calls [`apply_bond_resolution`] to perform the transitions.
//!
//! ## Feature-gate behaviour
//!
//! These functions are safe to call even when the anti-abuse bond feature
//! is disabled. `find_active_bonds_for_order` returns an empty set when
//! no bonds exist for the order, and both functions then no-op. This
//! preserves the Phase 1 invariant that a `null` payload + no bond rows
//! yields exactly the legacy behaviour.

use std::collections::HashSet;

use chrono::Utc;
use mostro_core::error::{
    CantDoReason,
    MostroError::{self, MostroCantDo},
};
use mostro_core::message::{BondResolution, Message, Payload};
use mostro_core::order::Order;
use sqlx::{Pool, Sqlite};
use tracing::{info, warn};
use uuid::Uuid;

use super::db::find_active_bonds_for_order;
use super::flow::release_bond;
use super::math::compute_node_share;
use super::model::Bond;
use super::types::{BondSlashReason, BondState};
use crate::config::settings::Settings;

/// Which trade-flow side a slash flag is targeting. Internal helper —
/// callers think in `BondResolution::slash_seller` / `slash_buyer`
/// terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Seller,
    Buyer,
}

/// Extract a [`BondResolution`] from an admin message, defaulting to
/// "release all bonds" when the payload is absent, `null`, or carries
/// a different shape.
///
/// `MessageKind::verify` upstream already rejects `BondResolution` on
/// actions other than `AdminSettle` / `AdminCancel` (see `mostro-core`
/// 0.11.0 §verify), so by the time we're here the only "wrong shape"
/// case is the legitimate "no payload at all" — admin clients on the
/// pre-Phase-2 wire format. Defaulting to `false`/`false` is the
/// behaviour Phase 1 had.
pub fn extract_bond_resolution(msg: &Message) -> BondResolution {
    match &msg.get_inner_message_kind().payload {
        Some(Payload::BondResolution(br)) => br.clone(),
        _ => BondResolution {
            slash_seller: false,
            slash_buyer: false,
        },
    }
}

/// Pre-flight validation for a [`BondResolution`] payload.
///
/// Must be called **before** any trade-side mutation
/// (`settle_seller_hold_invoice`, `cancel_hold_invoice`,
/// `update_order_event`, …): if validation fails, the admin handler
/// returns `MostroCantDo(InvalidPayload)` and the trade itself is not
/// settled or cancelled. The solver is expected to resend a corrected
/// directive — see §7.3 step 3 of `docs/ANTI_ABUSE_BOND.md`.
///
/// Returns:
/// - `Ok(())` when no slash is requested (`null` payload ≡ both flags
///   `false`), or when every requested slash maps to a side with a
///   `Locked` bond row.
/// - `Err(MostroCantDo(InvalidPayload))` when any requested slash targets
///   a side that has no `Locked` bond. This naturally covers
///   "feature disabled / `apply_to=none` / no bond posted" without a
///   separate config check — the absence of the row is the signal.
pub async fn validate_bond_resolution(
    pool: &Pool<Sqlite>,
    order: &Order,
    resolution: &BondResolution,
) -> Result<(), MostroError> {
    if !resolution.slash_seller && !resolution.slash_buyer {
        return Ok(());
    }
    let bonds = find_active_bonds_for_order(pool, order.id).await?;
    if resolution.slash_seller && resolve_locked_bond(order, &bonds, Side::Seller).is_none() {
        return Err(MostroCantDo(CantDoReason::InvalidPayload));
    }
    if resolution.slash_buyer && resolve_locked_bond(order, &bonds, Side::Buyer).is_none() {
        return Err(MostroCantDo(CantDoReason::InvalidPayload));
    }
    Ok(())
}

/// Apply a validated [`BondResolution`] to every active bond on the order.
///
/// For each currently active bond:
/// - if the bond's owner is on the slashed side(s), perform a CAS
///   `state='locked' → state='pending-payout'` that also writes
///   `slashed_reason`, `slashed_at`, and `node_share_sats` in the same
///   statement. The split snapshot is intentionally frozen at this
///   moment — Phase 3's payout job never reads `slash_node_share_pct`
///   from config; it reads `node_share_sats` from the row.
/// - otherwise, release the bond exactly as Phase 1 did
///   (`cancel_hold_invoice` if applicable, then state = `Released`).
///
/// Idempotent: a bond that already moved out of `Locked` (e.g. a
/// duplicate admin call or a concurrent path) is left untouched by the
/// CAS, and the loop continues to the next bond. The `release_bond`
/// call is itself idempotent for terminal states.
///
/// `reason` is `LostDispute` in Phase 2 (called from admin handlers);
/// Phase 4 (timeout slash) will reuse this helper with `Timeout`.
pub async fn apply_bond_resolution(
    pool: &Pool<Sqlite>,
    order: &Order,
    resolution: &BondResolution,
    reason: BondSlashReason,
) -> Result<(), MostroError> {
    let bonds = find_active_bonds_for_order(pool, order.id).await?;
    if bonds.is_empty() {
        return Ok(());
    }

    let mut slashed_ids: HashSet<Uuid> = HashSet::new();
    if resolution.slash_seller {
        if let Some(bond) = resolve_locked_bond(order, &bonds, Side::Seller) {
            slashed_ids.insert(bond.id);
        }
        // No-op if a Locked bond is missing: validation should have run
        // before any trade-side mutation. Reaching here with a missing
        // bond means a concurrent path (release, slash, expiry) raced
        // between validate and apply — letting the loop fall through to
        // the release branch on whatever remains is the safe outcome.
    }
    if resolution.slash_buyer {
        if let Some(bond) = resolve_locked_bond(order, &bonds, Side::Buyer) {
            slashed_ids.insert(bond.id);
        }
    }

    // Snapshot the split percentage *once* per call. Phase 3 will read
    // `node_share_sats` off each row; we never recompute it after the
    // transition.
    let node_share_pct = Settings::get_bond().map_or(0.0, |c| c.slash_node_share_pct);

    for bond in bonds.iter() {
        if slashed_ids.contains(&bond.id) {
            slash_one(pool, bond, reason, node_share_pct).await;
        } else {
            // Non-slashed bonds on the same order: release with the
            // Phase 1 contract. `release_bond` is best-effort and
            // tolerant of transient LND failures.
            if let Err(e) = release_bond(pool, bond).await {
                warn!(
                    bond_id = %bond.id,
                    order_id = %order.id,
                    "apply_bond_resolution: release_bond failed: {}", e
                );
            }
        }
    }

    Ok(())
}

/// Single-bond `Locked → PendingPayout` transition with snapshot fields.
///
/// Uses a CAS `WHERE id = ? AND state = 'locked'` so a duplicate admin
/// call or a concurrent transition cannot overwrite a row that already
/// moved on. The CAS write is the only place `slashed_reason`,
/// `slashed_at`, and `node_share_sats` are populated for a `LostDispute`
/// row, which is what makes the split snapshot deterministic across
/// restarts and config changes.
async fn slash_one(pool: &Pool<Sqlite>, bond: &Bond, reason: BondSlashReason, node_share_pct: f64) {
    let node_share_sats = compute_node_share(bond.amount_sats, node_share_pct);
    let now = Utc::now().timestamp();
    let result = sqlx::query(
        "UPDATE bonds \
           SET state = ?, slashed_reason = ?, slashed_at = ?, node_share_sats = ? \
         WHERE id = ? AND state = ?",
    )
    .bind(BondState::PendingPayout.to_string())
    .bind(reason.to_string())
    .bind(now)
    .bind(node_share_sats)
    .bind(bond.id)
    .bind(BondState::Locked.to_string())
    .execute(pool)
    .await;
    match result {
        Ok(r) if r.rows_affected() == 1 => {
            info!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                reason = %reason,
                node_share_sats,
                counterparty_share_sats = bond.amount_sats - node_share_sats,
                "Bond transitioned to PendingPayout"
            );
        }
        Ok(_) => {
            // The bond moved out of `Locked` between our enumerate and
            // our CAS. Phase 3 will not pick it up (only `PendingPayout`
            // qualifies for payout); any prior transition (e.g. a
            // concurrent release) owns the row now.
            warn!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                current_state = %bond.state,
                "slash CAS no-op (bond state changed concurrently)"
            );
        }
        Err(e) => {
            warn!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                "slash CAS DB error: {}", e
            );
        }
    }
}

/// Resolve a buyer/seller slash flag to the matching `Locked` bond row,
/// if any. The mapping uses the §3.1 buyer-side → trade-pubkey lookup
/// on the order, then filters bonds by `pubkey` and `state = Locked`.
///
/// Returns `None` when the side has no `Locked` bond row — either no
/// bond exists on this order for that pubkey, the bond already moved
/// out of `Locked` (e.g. into `Released` or `PendingPayout`), or the
/// side's pubkey is unset on the order. Validation treats `None` as
/// "InvalidPayload"; `apply` treats it as a benign skip.
fn resolve_locked_bond<'a>(order: &Order, bonds: &'a [Bond], side: Side) -> Option<&'a Bond> {
    let target_pubkey = match side {
        Side::Seller => order.seller_pubkey.as_deref()?,
        Side::Buyer => order.buyer_pubkey.as_deref()?,
    };
    let locked = BondState::Locked.to_string();
    bonds
        .iter()
        .find(|b| b.pubkey == target_pubkey && b.state == locked)
}

#[cfg(test)]
mod tests {
    use mostro_core::message::{Action, Message, Payload};
    use mostro_core::order::{Kind, Order, Status};
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::{Pool, Sqlite};

    use super::*;
    use crate::app::bond::model::Bond;
    use crate::app::bond::types::{BondRole, BondSlashReason, BondState};

    async fn setup_pool() -> Pool<Sqlite> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .expect("open in-memory sqlite");
        sqlx::query(include_str!(
            "../../../migrations/20221222153301_orders.sql"
        ))
        .execute(&pool)
        .await
        .expect("orders migration");
        for stmt in include_str!("../../../migrations/20251126120000_dev_fee.sql")
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.lines().all(|l| l.trim_start().starts_with("--")))
        {
            sqlx::query(stmt)
                .execute(&pool)
                .await
                .expect("dev_fee migration");
        }
        sqlx::query(include_str!(
            "../../../migrations/20260423120000_anti_abuse_bond.sql"
        ))
        .execute(&pool)
        .await
        .expect("bonds migration");
        pool
    }

    async fn insert_order_row(pool: &Pool<Sqlite>, order: &Order) {
        sqlx::query(
            r#"INSERT INTO orders (
                id, kind, event_id, status, premium, payment_method,
                amount, fiat_code, fiat_amount, created_at, expires_at,
                seller_pubkey, buyer_pubkey
            ) VALUES (?, ?, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(order.id)
        .bind(&order.kind)
        .bind(order.id.simple().to_string())
        .bind(&order.status)
        .bind(&order.payment_method)
        .bind(order.amount)
        .bind(&order.fiat_code)
        .bind(order.fiat_amount)
        .bind(order.created_at)
        .bind(order.expires_at)
        .bind(order.seller_pubkey.as_deref())
        .bind(order.buyer_pubkey.as_deref())
        .execute(pool)
        .await
        .expect("insert order");
    }

    fn fixture_order(kind: Kind, seller_pk: &str, buyer_pk: &str) -> Order {
        Order {
            id: Uuid::new_v4(),
            kind: kind.to_string(),
            status: Status::Dispute.to_string(),
            seller_pubkey: Some(seller_pk.to_string()),
            buyer_pubkey: Some(buyer_pk.to_string()),
            amount: 100_000,
            fiat_code: "USD".to_string(),
            fiat_amount: 10,
            payment_method: "lightning".to_string(),
            created_at: Utc::now().timestamp(),
            expires_at: Utc::now().timestamp() + 3600,
            ..Order::default()
        }
    }

    async fn insert_bond(
        pool: &Pool<Sqlite>,
        order_id: Uuid,
        pubkey: &str,
        state: BondState,
    ) -> Bond {
        let mut b = Bond::new_requested(order_id, pubkey.to_string(), BondRole::Taker, 10_000);
        b.state = state.to_string();
        // No hash → release_bond skips the LND cancel branch entirely
        // (see `release_bond` in flow.rs).
        b.hash = None;
        sqlx_crud::Crud::create(b.clone(), pool).await.unwrap();
        b
    }

    fn taker_pk() -> &'static str {
        "1111111111111111111111111111111111111111111111111111111111111111"
    }
    fn maker_pk() -> &'static str {
        "2222222222222222222222222222222222222222222222222222222222222222"
    }

    fn order_msg_with(payload: Option<Payload>) -> Message {
        Message::new_order(
            Some(Uuid::new_v4()),
            None,
            None,
            Action::AdminSettle,
            payload,
        )
    }

    #[test]
    fn extract_returns_default_when_payload_absent() {
        // The pre-Phase-2 admin client sends `payload: None`. The
        // extractor must default to "release all bonds" so Phase 1
        // behaviour is preserved bit-for-bit.
        let msg = order_msg_with(None);
        let br = extract_bond_resolution(&msg);
        assert!(!br.slash_seller);
        assert!(!br.slash_buyer);
    }

    #[test]
    fn extract_returns_default_for_unrelated_payload_shapes() {
        // A payload of the wrong shape is upstream-rejected by verify
        // for AdminSettle/Cancel, but defending here means an exotic
        // future variant cannot accidentally activate a slash.
        let msg = order_msg_with(Some(Payload::TextMessage("hi".into())));
        let br = extract_bond_resolution(&msg);
        assert!(!br.slash_seller);
        assert!(!br.slash_buyer);
    }

    #[test]
    fn extract_returns_payload_when_present() {
        let payload = Payload::BondResolution(BondResolution {
            slash_seller: true,
            slash_buyer: false,
        });
        let msg = order_msg_with(Some(payload));
        let br = extract_bond_resolution(&msg);
        assert!(br.slash_seller);
        assert!(!br.slash_buyer);
    }

    #[tokio::test]
    async fn validate_null_payload_passes_with_no_bonds() {
        // null/false-false payload + no bond rows = legacy Phase 1
        // behaviour. Must pass without touching the DB beyond the
        // (empty) lookup.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: false,
        };
        validate_bond_resolution(&pool, &order, &res).await.unwrap();
    }

    #[tokio::test]
    async fn validate_slash_buyer_passes_when_buyer_has_locked_bond() {
        // sell-order: taker is buyer, with a Locked bond.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };
        validate_bond_resolution(&pool, &order, &res).await.unwrap();
    }

    #[tokio::test]
    async fn validate_slash_seller_on_sell_apply_to_take_rejects() {
        // Spec test: cancel + slash_seller on a sell-order with
        // apply_to=take. The seller is the maker and has no bond (only
        // taker bonds in Phase 2). Must fail before any trade mutation.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        // Taker has a Locked bond, but the seller (maker) has none.
        insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let res = BondResolution {
            slash_seller: true,
            slash_buyer: false,
        };
        let err = validate_bond_resolution(&pool, &order, &res)
            .await
            .unwrap_err();
        assert!(
            matches!(err, MostroCantDo(CantDoReason::InvalidPayload)),
            "expected CantDo(InvalidPayload), got {err:?}"
        );
    }

    #[tokio::test]
    async fn validate_rejects_when_bond_table_is_empty() {
        // Feature-disabled-style scenario: no bond rows at all. Any
        // slash flag must be rejected.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };
        let err = validate_bond_resolution(&pool, &order, &res)
            .await
            .unwrap_err();
        assert!(matches!(err, MostroCantDo(CantDoReason::InvalidPayload)));
    }

    #[tokio::test]
    async fn apply_null_payload_releases_all_active_bonds() {
        // payload=null preserves Phase 1: any active bond on the order
        // is released. Bond table contents are exercised; LND is not
        // touched because the bond has `hash = None`.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;

        let res = BondResolution {
            slash_seller: false,
            slash_buyer: false,
        };
        apply_bond_resolution(&pool, &order, &res, BondSlashReason::LostDispute)
            .await
            .unwrap();

        let row: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.0,
            BondState::Released.to_string(),
            "null payload must release, not slash"
        );
    }

    #[tokio::test]
    async fn apply_slash_buyer_on_sell_order_transitions_taker_bond() {
        // Spec example: settle + slash_buyer=true on a sell-order. Taker
        // is the buyer; their Locked bond enters PendingPayout with the
        // split snapshot persisted on the row.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };
        apply_bond_resolution(&pool, &order, &res, BondSlashReason::LostDispute)
            .await
            .unwrap();

        let row: (String, Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT state, slashed_reason, slashed_at, node_share_sats \
             FROM bonds WHERE id = ?",
        )
        .bind(bond.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, BondState::PendingPayout.to_string());
        assert_eq!(row.1.as_deref(), Some("lost-dispute"));
        assert!(row.2.unwrap() > 0, "slashed_at must be set");
        // No `[anti_abuse_bond]` block in test config → Settings::get_bond
        // returns None and the helper falls back to pct=0.0 (legacy
        // winner-takes-all). The load-bearing assertion is that the
        // snapshot is *persisted* on the row; the specific value is a
        // function of config and is exercised by math.rs tests.
        assert_eq!(row.3, Some(0));
    }

    #[tokio::test]
    async fn apply_is_idempotent_on_already_pending_payout() {
        // A duplicate admin call (or a slash CAS racing with itself)
        // must not rebump `slashed_at` or rewrite `node_share_sats`.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };

        apply_bond_resolution(&pool, &order, &res, BondSlashReason::LostDispute)
            .await
            .unwrap();
        let first: (Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT slashed_at, node_share_sats FROM bonds WHERE id = ?")
                .bind(bond.id)
                .fetch_one(&pool)
                .await
                .unwrap();

        // Pretend a duplicate admin DM arrived a second later.
        std::thread::sleep(std::time::Duration::from_secs(1));
        apply_bond_resolution(&pool, &order, &res, BondSlashReason::LostDispute)
            .await
            .unwrap();
        let second: (Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT slashed_at, node_share_sats FROM bonds WHERE id = ?")
                .bind(bond.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            first, second,
            "second apply must not rebump slashed_at / node_share_sats"
        );
    }

    #[tokio::test]
    async fn apply_with_no_bond_rows_is_noop() {
        // Feature-disabled-shaped path: bond table is empty. The helper
        // must complete without error and without writing anything.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: false,
        };
        apply_bond_resolution(&pool, &order, &res, BondSlashReason::LostDispute)
            .await
            .unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM bonds")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0);
    }
}
