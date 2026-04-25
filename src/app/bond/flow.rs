//! Bond lifecycle wiring (Phase 1).
//!
//! Phase 1 adds a single guarantee: when the feature is enabled and the
//! taker side is in scope (`apply_to ∈ {take, both}`), a taker is asked to
//! lock a Lightning hold invoice as a bond before the trade flow starts;
//! and on **every** exit — happy path, unilateral cancel, cooperative
//! cancel, admin action, scheduler timeout — the bond is **released**.
//!
//! Slashing is intentionally absent: it lands in Phase 2+. This means
//! operators can flip `enabled = true` in staging and exercise hold-invoice
//! custody end-to-end without any user funds at risk if Mostro mis-judges
//! the situation.
//!
//! Protocol note: `mostro-core` 0.10.0 does not yet expose
//! `Action::AddBondInvoice` / `Status::WaitingTakerBond`. Phase 1 takes the
//! "Alternative" path documented in §6.2 of `docs/ANTI_ABUSE_BOND.md`:
//! orders stay in `Status::Pending` while waiting for the bond, and the
//! bond bolt11 ships to the taker as a regular `Action::PayInvoice` (the
//! semantics — "pay this Lightning invoice" — are an exact match). Bond
//! state lives entirely in the `bonds` table; clients identify the
//! invoice as a bond by its hash, which differs from the trade hold
//! invoice that follows once the bond is locked. The dedicated action /
//! status will land alongside the corresponding `mostro-core` release in a
//! later phase.

use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use fedimint_tonic_lnd::lnrpc::invoice::InvoiceState;
use mostro_core::error::{MostroError, MostroError::MostroInternalErr, ServiceError};
use mostro_core::prelude::*;
use nostr_sdk::nostr::hashes::hex::FromHex;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tokio::sync::mpsc::channel;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::settings::Settings;
use crate::lightning::{InvoiceMessage, LndConnector};
use crate::util::{
    bytes_to_string, enqueue_order_msg, get_keys, set_waiting_invoice_status, show_hold_invoice,
};

use super::db::{create_bond, find_active_bonds, find_active_bonds_for_order, find_bond_by_hash};
use super::math::compute_bond_amount;
use super::model::Bond;
use super::types::{BondRole, BondState};

/// True when the configuration requires the **taker** to post a bond.
///
/// This is the single Phase 1 gate. Every bond touchpoint in the take
/// flow asks this question first, so a misconfigured node (no
/// `[anti_abuse_bond]` block at all) behaves exactly like before.
pub fn taker_bond_required() -> bool {
    Settings::get_bond()
        .filter(|cfg| cfg.enabled)
        .is_some_and(|cfg| cfg.apply_to.applies_to_taker())
}

/// Create a hold invoice for the taker's bond, persist a `Bond` row in
/// `Requested`, ship the bolt11 to the taker, and start the LND
/// subscriber that flips the row to `Locked` once the taker pays.
///
/// On any failure inside this function the bond row may exist in
/// `Requested` with no LND counterpart — that's fine: Phase 1's
/// "always release" guarantee covers it on the next exit.
pub async fn request_taker_bond(
    pool: &Pool<Sqlite>,
    order: &Order,
    taker_pubkey: PublicKey,
    request_id: Option<u64>,
    trade_index: Option<i64>,
) -> Result<Bond, MostroError> {
    let cfg = Settings::get_bond().ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(
            "anti_abuse_bond block is missing while bond was deemed required".into(),
        ))
    })?;

    let amount = compute_bond_amount(order.amount, cfg);
    let memo = format!("Bond for Mostro order {}", order.id);

    let mut ln_client = LndConnector::new().await?;
    let (invoice_resp, preimage, hash) = ln_client
        .create_hold_invoice(&memo, amount)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::HoldInvoiceError(e.to_string())))?;

    let mut bond = Bond::new_requested(order.id, taker_pubkey.to_string(), BondRole::Taker, amount);
    bond.hash = Some(bytes_to_string(&hash));
    bond.preimage = Some(bytes_to_string(&preimage));
    bond.payment_request = Some(invoice_resp.payment_request.clone());

    let bond = create_bond(pool, bond).await?;

    info!(
        "Bond requested: bond_id={} order_id={} role={} amount_sats={}",
        bond.id, order.id, bond.role, bond.amount_sats
    );

    // Phase-1 alternative path (see module-level doc): the bond bolt11
    // ships as a regular `PayInvoice`. The `SmallOrder` echoes the order
    // id so a bond-aware client can correlate — and a non-bond-aware
    // client just sees an extra invoice to pay before the trade.
    let order_kind = order.get_order_kind().map_err(MostroInternalErr)?;
    let bond_small = SmallOrder::new(
        Some(order.id),
        Some(order_kind),
        Some(Status::Pending),
        amount,
        order.fiat_code.clone(),
        order.min_amount,
        order.max_amount,
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        None,
        None,
        None,
        None,
        None,
    );

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::PayInvoice,
        Some(Payload::PaymentRequest(
            Some(bond_small),
            invoice_resp.payment_request,
            None,
        )),
        taker_pubkey,
        trade_index,
    )
    .await;

    bond_invoice_subscribe(hash, request_id).await?;

    Ok(bond)
}

/// Release a single bond: cancel the hold invoice (best-effort) and
/// transition the row to `Released`.
///
/// **Idempotent.** A bond already in a terminal state (`Released`,
/// `Slashed`, `Failed`) is a no-op. This matters because Phase 1 wires
/// release into every exit, and the same bond can plausibly be hit by
/// more than one path (e.g. cooperative cancel after the LND subscriber
/// already saw `Canceled`).
pub async fn release_bond(pool: &Pool<Sqlite>, bond: &Bond) -> Result<(), MostroError> {
    // Parse `state` once into the enum so callers don't depend on the
    // `Display` form for control flow (and a malformed value short-
    // circuits to "no-op" instead of falsely transitioning).
    let state = BondState::from_str(&bond.state).map_err(|e| {
        MostroInternalErr(ServiceError::UnexpectedError(format!(
            "Bond {} has unparseable state {:?}: {}",
            bond.id, bond.state, e
        )))
    })?;
    if state.is_terminal() {
        return Ok(());
    }

    if let Some(hash) = bond.hash.as_ref() {
        match LndConnector::new().await {
            Ok(mut ln) => {
                if let Err(e) = ln.cancel_hold_invoice(hash).await {
                    // Hold invoice already canceled / unknown to LND is the
                    // common race with the subscriber; we still want the row
                    // to land in `Released` so callers can move on.
                    warn!(
                        "Bond {} cancel_hold_invoice failed: {} — marking Released anyway",
                        bond.id, e
                    );
                }
            }
            Err(e) => {
                warn!(
                    "Bond {} could not connect to LND for cancel: {} — marking Released anyway",
                    bond.id, e
                );
            }
        }
    }

    let mut updated = bond.clone();
    updated.state = BondState::Released.to_string();
    updated.released_at = Some(Utc::now().timestamp());
    let id = updated.id;
    let order_id = updated.order_id;
    updated
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    info!(
        "Bond {} released for order {} (was state={})",
        id, order_id, bond.state
    );
    Ok(())
}

/// Release every active (`Requested` or `Locked`) bond attached to an
/// order. Designed to be the **single** call sites use from each exit
/// path — the gate, the lookup, and the per-row release are all here.
///
/// Returns `Ok(())` when the feature is disabled or no active bonds
/// exist; never fails the caller for individual bond failures (those
/// are logged and the loop continues).
pub async fn release_bonds_for_order(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
) -> Result<(), MostroError> {
    if !Settings::is_bond_enabled() {
        return Ok(());
    }

    let bonds = find_active_bonds_for_order(pool, order_id).await?;
    for bond in bonds.iter() {
        if let Err(e) = release_bond(pool, bond).await {
            warn!("Failed to release bond {}: {}", bond.id, e);
        }
    }
    Ok(())
}

/// Best-effort release helper for the Phase 1 exit paths.
///
/// Every order-exit flow (release, cancel, admin actions, scheduler
/// timeouts) wants the same shape: try to release the bond, and on
/// failure log a warning tagged with the call site — never propagate.
/// Centralising the pattern keeps each call site to a single line and
/// guarantees consistent log structure for operators.
pub async fn release_bonds_for_order_or_warn(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    context: &'static str,
) {
    if let Err(e) = release_bonds_for_order(pool, order_id).await {
        warn!("{context}: bond release failed for {}: {}", order_id, e);
    }
}

/// Spawn the LND subscriber for a bond hold invoice. The subscriber
/// transitions the bond row through `Locked` / `Released` based on the
/// invoice state and, on `Locked`, resumes the original take flow.
///
/// Mirrors the structure of `crate::util::invoice_subscribe` so restart
/// resilience can later reuse the same shape.
pub async fn bond_invoice_subscribe(
    hash: Vec<u8>,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    let mut ln_client = LndConnector::new().await?;
    let (tx, mut rx) = channel::<InvoiceMessage>(100);

    tokio::spawn(async move {
        if let Err(e) = ln_client.subscribe_invoice(hash, tx).await {
            warn!("Bond invoice subscriber ended with error: {e}");
        }
    });

    let pool = crate::config::settings::get_db_pool();

    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let hash_hex = bytes_to_string(msg.hash.as_ref());
            match msg.state {
                InvoiceState::Accepted => {
                    if let Err(e) = on_bond_invoice_accepted(&hash_hex, &pool, request_id).await {
                        warn!("Bond invoice accepted handler error: {e}");
                    }
                }
                InvoiceState::Canceled => {
                    if let Err(e) = on_bond_invoice_canceled(&hash_hex, &pool).await {
                        warn!("Bond invoice canceled handler error: {e}");
                    }
                }
                InvoiceState::Settled => {
                    info!("Bond hash {hash_hex}: invoice settled");
                }
                InvoiceState::Open => {
                    info!("Bond hash {hash_hex}: invoice open (waiting for payment)");
                }
            }
        }
    });

    Ok(())
}

/// Restart hook: re-subscribe to every bond that was still active when
/// the daemon stopped. Called from `main` next to `find_held_invoices`.
pub async fn resubscribe_active_bonds(pool: &Arc<Pool<Sqlite>>) -> Result<(), MostroError> {
    if !Settings::is_bond_enabled() {
        return Ok(());
    }
    let bonds = find_active_bonds(pool.as_ref()).await?;
    for bond in bonds.into_iter() {
        if let Some(hash) = bond.hash.as_ref() {
            // Hex string back to bytes for LND.
            match Vec::<u8>::from_hex(hash) {
                Ok(bytes) => {
                    if let Err(e) = bond_invoice_subscribe(bytes, None).await {
                        warn!("Failed to resubscribe bond {}: {}", bond.id, e);
                    } else {
                        info!("Resubscribed bond {} (state={})", bond.id, bond.state);
                    }
                }
                Err(e) => warn!("Bond {} has malformed hash: {}", bond.id, e),
            }
        }
    }
    Ok(())
}

/// Subscriber callback for `InvoiceState::Accepted`: bond is locked.
///
/// Transitions the row to `Locked` and resumes the original take flow
/// (creates the trade hold invoice / asks the buyer for a payout
/// invoice, depending on the side).
async fn on_bond_invoice_accepted(
    hash: &str,
    pool: &Pool<Sqlite>,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    let bond = match find_bond_by_hash(pool, hash).await? {
        Some(b) => b,
        None => {
            warn!("Bond invoice accepted for unknown hash {hash}");
            return Ok(());
        }
    };

    // Concurrent subscriber firings (LND can emit Accepted more than once
    // on reconnect, and the restart-time resubscriber re-attaches another
    // listener) must not both run the take continuation. The conditional
    // UPDATE is the single point of synchronisation: only the row that
    // actually wins the `requested` → `locked` race continues here.
    let now = Utc::now().timestamp();
    let result =
        sqlx::query("UPDATE bonds SET state = ?, locked_at = ? WHERE id = ? AND state = ?")
            .bind(BondState::Locked.to_string())
            .bind(now)
            .bind(bond.id)
            .bind(BondState::Requested.to_string())
            .execute(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if result.rows_affected() == 0 {
        // Either another subscriber already locked the bond (idempotent
        // — nothing to do), or the row moved to a non-Requested state
        // through a concurrent release path (also fine: the take won't
        // resume on a released bond). Log only when surprising.
        if !matches!(bond.state.as_str(), s if s == BondState::Requested.to_string()
            || s == BondState::Locked.to_string())
        {
            warn!(
                "Bond {} accepted but state was {} — ignoring",
                bond.id, bond.state
            );
        }
        return Ok(());
    }

    info!("Bond {} locked for order {}", bond.id, bond.order_id);

    let order = Order::by_id(pool, bond.order_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
        .ok_or_else(|| {
            MostroInternalErr(ServiceError::UnexpectedError(format!(
                "Bond {} references missing order {}",
                bond.id, bond.order_id
            )))
        })?;

    let my_keys = get_keys()?;
    resume_take_after_bond(pool, order, &my_keys, request_id).await
}

/// Subscriber callback for `InvoiceState::Canceled`: bond never locked
/// (taker abandoned the invoice, or LND auto-canceled on expiration).
///
/// Phase 1 keeps the order untouched: it stays `Pending` with the taker
/// fields populated. The maker's order remains discoverable via the
/// existing Nostr event. A follow-up phase (or operator action) can
/// reset the order if needed; for Phase 1, "always release" is the only
/// guarantee we owe.
async fn on_bond_invoice_canceled(hash: &str, pool: &Pool<Sqlite>) -> Result<(), MostroError> {
    let bond = match find_bond_by_hash(pool, hash).await? {
        Some(b) => b,
        None => return Ok(()),
    };

    if BondState::from_str(&bond.state)
        .map(|s| s.is_terminal())
        .unwrap_or(false)
    {
        return Ok(());
    }

    let mut updated = bond.clone();
    updated.state = BondState::Released.to_string();
    updated.released_at = Some(Utc::now().timestamp());
    updated
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    info!(
        "Bond {} marked Released after LND cancel (order {})",
        bond.id, bond.order_id
    );
    Ok(())
}

/// Resume the take flow after the bond locks.
///
/// The taker side already populated all trade fields on the order before
/// requesting the bond, so this function only needs to drive the trade
/// hold invoice / payout-invoice request that `take_*_action` deferred.
async fn resume_take_after_bond(
    pool: &Pool<Sqlite>,
    mut order: Order,
    my_keys: &Keys,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    let kind = order.get_order_kind().map_err(MostroInternalErr)?;
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;

    match kind {
        // Buy order → taker = seller, no buyer-invoice required up front:
        // mirror the post-take path in take_buy_action.
        mostro_core::order::Kind::Buy => {
            show_hold_invoice(
                my_keys,
                None,
                &buyer_pubkey,
                &seller_pubkey,
                order,
                request_id,
            )
            .await
        }
        // Sell order → taker = buyer. If the buyer included an invoice in
        // the take message we already persisted it on `order.buyer_invoice`;
        // otherwise we ask for one. This mirrors take_sell_action.
        mostro_core::order::Kind::Sell => {
            if order.buyer_invoice.is_some() {
                let payment_request = order.buyer_invoice.clone();
                show_hold_invoice(
                    my_keys,
                    payment_request,
                    &buyer_pubkey,
                    &seller_pubkey,
                    order,
                    request_id,
                )
                .await
            } else {
                set_waiting_invoice_status(&mut order, buyer_pubkey, request_id)
                    .await
                    .map_err(|_| MostroInternalErr(ServiceError::UpdateOrderStatusError))?;
                let order_updated =
                    crate::util::update_order_event(my_keys, Status::WaitingBuyerInvoice, &order)
                        .await
                        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
                order_updated
                    .update(pool)
                    .await
                    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::bond::types::BondRole;
    use sqlx::sqlite::SqlitePoolOptions;

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
        sqlx::query(include_str!(
            "../../../migrations/20260423120000_anti_abuse_bond.sql"
        ))
        .execute(&pool)
        .await
        .expect("bonds migration");
        pool
    }

    async fn insert_order(pool: &Pool<Sqlite>, id: Uuid) {
        sqlx::query(
            r#"INSERT INTO orders (
                id, kind, event_id, status, premium, payment_method,
                amount, fiat_code, fiat_amount, created_at, expires_at
            ) VALUES (?, 'buy', ?, 'pending', 0, 'ln', 1000, 'USD', 10, 0, 0)"#,
        )
        .bind(id)
        .bind(id.simple().to_string())
        .execute(pool)
        .await
        .expect("insert order");
    }

    fn make_bond(order_id: Uuid, state: BondState) -> Bond {
        let mut b = Bond::new_requested(order_id, "a".repeat(64), BondRole::Taker, 1_500);
        b.state = state.to_string();
        b.hash = Some("c".repeat(64));
        b
    }

    #[tokio::test]
    async fn release_bond_is_idempotent_for_terminal_states() {
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;
        let bond = create_bond(&pool, make_bond(order_id, BondState::Released))
            .await
            .unwrap();

        // No LND, no panic: idempotent on terminal states.
        release_bond(&pool, &bond).await.unwrap();

        let after = find_bond_by_hash(&pool, &"c".repeat(64))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, "released");
        assert_eq!(after.released_at, bond.released_at);
    }

    #[tokio::test]
    async fn release_bonds_for_order_no_op_when_disabled() {
        // No `[anti_abuse_bond]` block in test settings → feature off.
        // Function must succeed without touching LND or DB beyond a
        // configuration check.
        let pool = setup_pool().await;
        // Even with active bonds in the DB, the gate keeps us out.
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;
        let _ = create_bond(&pool, make_bond(order_id, BondState::Locked))
            .await
            .unwrap();

        // Settings::is_bond_enabled() reads MOSTRO_CONFIG which is unset
        // in the unit-test harness → returns false. Verify the call path
        // is a clean no-op.
        release_bonds_for_order(&pool, order_id).await.unwrap();

        // Bond untouched.
        let active = find_active_bonds_for_order(&pool, order_id).await.unwrap();
        assert_eq!(active.len(), 1);
    }

    #[tokio::test]
    async fn release_bond_without_hash_marks_released() {
        // A `Requested` bond with no hash yet (e.g. failure between
        // `new_requested` and `create_hold_invoice`) must still be
        // releasable: the row transitions to `Released` and no LND call
        // is attempted.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;
        let mut bond = make_bond(order_id, BondState::Requested);
        bond.hash = None;
        let bond = create_bond(&pool, bond).await.unwrap();

        release_bond(&pool, &bond).await.unwrap();

        let active = find_active_bonds_for_order(&pool, order_id).await.unwrap();
        assert!(active.is_empty(), "bond should no longer be active");
    }

    #[test]
    fn taker_bond_required_is_false_without_config() {
        // No global config initialized in unit tests → gate must be off.
        // Guarantees that all bond touchpoints are inert in the absence
        // of an `[anti_abuse_bond]` block.
        assert!(!taker_bond_required());
    }
}
