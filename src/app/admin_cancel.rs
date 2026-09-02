use std::str::FromStr;

use crate::app::bond::{self, BondSlashReason};
use crate::app::context::AppContext;
use crate::db::{
    ensure_dispute_finalize_permission, find_dispute_by_order_id, is_assigned_solver,
    is_dispute_taken_by_admin,
};
use crate::lightning::LndConnector;
use crate::nip33::{create_dispute_event_tags, new_dispute_event};
use crate::util::{enqueue_order_msg, get_order, send_dm, update_order_event};
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use tracing::{error, info};

/// Admin-initiated order cancellation.
///
/// Allows authorized dispute solvers or admins to cancel an order and refund
/// any held Lightning invoice back to the seller. A still-`Pending` order
/// (no taker, no escrow) may instead be cancelled by the operator through
/// the daemon key — the gRPC `CancelOrder` path — see
/// [`admin_cancel_pending_order`].
///
/// # Parameters
///
/// * `ctx` - Application context containing DB pool, settings, and message queue
/// * `msg` - Incoming message with the order ID and request metadata
/// * `event` - Unwrapped NIP-59 message exposing `sender` (trade key, rumor
///   author) and `identity` (long-lived identity key, seal signer); admin
///   gating is performed against `event.identity`
/// * `my_keys` - Mostro daemon's signing keys
/// * `ln_client` - Lightning network client for hold invoice cancellation
///
/// # Side Effects
///
/// - Cancels Lightning hold invoice (if present)
/// - Updates order status to `CanceledByAdmin` in database
/// - Publishes updated order event to Nostr
/// - Sends direct messages to both buyer and seller
///
/// # Errors
///
/// Returns `MostroError` if:
/// - Solver is not assigned to the dispute
/// - A pre-trade (`Pending` / `WaitingTakerBond`) order is cancelled by
///   anything but the daemon key
/// - Order/dispute not found
/// - Lightning invoice cancellation fails
/// - Database update fails
/// - Nostr publish fails
pub async fn admin_cancel_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let order = get_order(&msg, pool).await?;

    // Operator cancel of a pre-trade order (`Pending`, or parked at
    // `WaitingTakerBond` while a taker is mid-bond — the same window the
    // maker's own cancel covers, see `cancel::cancel_action_generic`). There
    // is no dispute to be assigned to and no escrow to refund, so the solver
    // gates below do not apply; instead only the daemon key may do this —
    // which is exactly what the gRPC `CancelOrder` path synthesises as
    // `identity`. A solver acting over Nostr never carries the daemon key and
    // is refused. Used to drain the book (open range-order maker bonds)
    // ahead of a Lightning node migration instead of waiting for
    // `max_expiration_days`.
    if order.check_status(Status::Pending).is_ok()
        || order.check_status(Status::WaitingTakerBond).is_ok()
    {
        if event.identity != my_keys.public_key() {
            return Err(MostroCantDo(CantDoReason::NotAuthorized));
        }
        return admin_cancel_pending_order(pool, &order, my_keys, ln_client).await;
    }

    // Check if the solver is assigned to the order
    match is_assigned_solver(pool, &event.identity.to_string(), order.id).await {
        Ok(false) => {
            // Check if admin has taken over the dispute
            if is_dispute_taken_by_admin(pool, order.id, &my_keys.public_key().to_string()).await? {
                return Err(MostroCantDo(CantDoReason::DisputeTakenByAdmin));
            } else {
                return Err(MostroCantDo(CantDoReason::IsNotYourDispute));
            }
        }
        Err(e) => {
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                e.to_string(),
            )));
        }
        _ => {}
    }

    ensure_dispute_finalize_permission(
        pool,
        &event.identity.to_string(),
        &my_keys.public_key().to_string(),
        order.id,
    )
    .await?;

    // Was order cooperatively cancelled?
    if order.check_status(Status::CooperativelyCanceled).is_ok() {
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::CooperativeCancelAccepted,
            None,
            event.identity,
            msg.get_inner_message_kind().trade_index,
        )
        .await;

        return Ok(());
    }

    // Was order in dispute?
    if order.check_status(Status::Dispute).is_err() {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    // Phase 2: extract and validate the optional `BondResolution` payload
    // here — after the status guards above (which are non-destructive
    // early returns, so an admin retry against an already-cooperatively-
    // cancelled or out-of-dispute order still gets the prior status-
    // driven response) and before the LND `cancel_hold_invoice` on the
    // escrow below, which would otherwise be irreversible. On a
    // `slash_*=true` for a side with no `Locked` bond row we return
    // `CantDo(InvalidPayload)` and the trade does not cancel; the solver
    // resends a corrected directive. See `docs/ANTI_ABUSE_BOND.md` §7.3.
    let bond_resolution = bond::extract_bond_resolution(&msg);
    bond::validate_bond_resolution(pool, &order, &bond_resolution).await?;

    if let Some(hash) = order.hash.as_ref() {
        // We return funds to seller. A hold invoice that LND already
        // canceled (CLTV expiry refunded the seller long ago) or does not
        // know at all (the daemon now runs against a different node) is
        // not an error: the HTLC is verifiably gone and the dispute can
        // still be closed. Only a transient LND failure aborts the cancel.
        tolerate_gone_hold_invoice(&order.id, ln_client.cancel_hold_invoice(hash).await)?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }

    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order.id).await;

    // Get the creator of the dispute
    let dispute_initiator = match (order.seller_dispute, order.buyer_dispute) {
        (true, false) => "seller",
        (false, true) => "buyer",
        (_, _) => return Err(MostroInternalErr(ServiceError::DisputeEventError)),
    };

    if let Ok(mut d) = dispute {
        let dispute_id = d.id;
        let opened_at = d.created_at;
        // we update the dispute
        d.status = DisputeStatus::SellerRefunded.to_string();
        d.update(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        // We create a tag to show status of the dispute
        let tags = create_dispute_event_tags(
            DisputeStatus::SellerRefunded.to_string(),
            dispute_initiator,
            opened_at,
            ctx.settings().mostro.name.as_deref(),
        );
        // nip33 kind with dispute id as identifier (kind 38386 for disputes)
        let event = new_dispute_event(my_keys, "", dispute_id.to_string(), tags)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

        // Publish dispute event with update
        info!("Dispute event to be published: {event:#?}");

        let client = ctx.nostr_client();
        if let Err(e) = client.send_event(&event).await {
            error!("Failed to send dispute status event: {}", e);
        }
    }

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    let order_updated = update_order_event(my_keys, Status::CanceledByAdmin, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    order_updated
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    // We create a Message for cancel
    let message = Message::new_order(
        Some(order.id),
        request_id,
        msg.get_inner_message_kind().trade_index,
        Action::AdminCanceled,
        None,
    );

    let message = message
        .as_json()
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    // Message to admin
    send_dm(event.sender, my_keys, &message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            PublicKey::from_str(buyer.as_str())
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
        ),
        (None, _) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
        (_, None) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };
    send_dm(seller_pubkey, my_keys, &message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    send_dm(buyer_pubkey, my_keys, &message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Phase 2: apply the solver's `BondResolution` to the bond rows
    // (release-by-default when absent). The buyer/seller pubkeys on
    // the order row are immutable through the dispute cycle, so the
    // original `order` snapshot is the right context for resolving
    // sides to bonds. Slashed bonds have their hold invoices settled
    // immediately; the recipient payout to the winning counterparty
    // is still Phase 3's job.
    // #768: notify each slashed party with a best-effort `BondSlashed`
    // forfeiture notice, mirroring the timeout-slash path. Only confirmed
    // slashes are returned, so a dropped settle never produces an untruthful
    // notice and an idempotent retry never re-notifies.
    match bond::apply_bond_resolution(
        pool,
        ln_client,
        &order,
        &bond_resolution,
        BondSlashReason::LostDispute,
    )
    .await
    {
        Ok(slashed_rows) => {
            for slashed in &slashed_rows {
                bond::notify_bond_slashed(&order, slashed).await;
            }
        }
        Err(e) => {
            tracing::warn!(
                order_id = %order.id,
                "admin_cancel: bond resolution apply failed: {}", e
            );
        }
    }

    // Phase 6: a dispute resolution ends the range (no remainder is
    // republished), so resolve the maker bond at close — settle the parent
    // HTLC once and refund the unslashed remainder if any slice was slashed,
    // otherwise release. A no-op for non-range maker bonds and for orders
    // with no maker bond.
    if let Err(e) = bond::resolve_range_maker_bond_at_close(pool, ln_client, &order).await {
        tracing::warn!(
            order_id = %order.id,
            "admin_cancel: maker bond close failed: {}", e
        );
    }

    Ok(())
}

/// Map the result of `cancel_hold_invoice` on the escrow: an invoice that
/// is already canceled / unknown to the node counts as done (the HTLC is
/// not encumbered), the same classification the bond release uses
/// (`bond::flow::classify_cancel_error`). Transient failures propagate so
/// the solver retries once LND is back.
fn tolerate_gone_hold_invoice<T>(
    order_id: &uuid::Uuid,
    result: Result<T, MostroError>,
) -> Result<(), MostroError> {
    use crate::app::bond::flow::{classify_cancel_error, CancelOutcome};
    match result {
        Ok(_) => Ok(()),
        Err(e) => match classify_cancel_error(&e) {
            CancelOutcome::AlreadyDone => {
                info!(
                    "Order Id {}: hold invoice already canceled or unknown to LND ({}); closing anyway",
                    order_id, e
                );
                Ok(())
            }
            CancelOutcome::Transient => Err(e),
        },
    }
}

/// Operator cancel of a pre-trade (`Pending` / `WaitingTakerBond`) order
/// (see the gate in [`admin_cancel_action`]). Mirrors the maker's own pending cancel
/// (`cancel::cancel_pending_order_from_maker`): publish the replaceable
/// event with `CanceledByAdmin`, persist it with a pre-trade CAS so a take
/// that commits concurrently wins, then notify the maker and any bonded
/// prospective taker, release the taker bonds and resolve the maker bond at
/// range close.
async fn admin_cancel_pending_order(
    pool: &sqlx::SqlitePool,
    order: &Order,
    my_keys: &Keys,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    let order_updated = update_order_event(my_keys, Status::CanceledByAdmin, order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let won = crate::db::cas_pretrade_order_status(
        pool,
        order_updated.id,
        Status::CanceledByAdmin,
        &order_updated.event_id,
    )
    .await?;
    if !won {
        // A take committed while we were publishing: the order has left the
        // pre-trade window and now carries escrow. Put the winning state back
        // on Nostr and tell the operator to look again.
        crate::util::republish_winning_state_after_cas_miss(pool, my_keys, order_updated.id).await;
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }
    info!("Order Id {}: pending order cancelled by operator", order.id);

    let maker = order.get_creator_pubkey().map_err(MostroInternalErr)?;
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::AdminCanceled,
        None,
        maker,
        None,
    )
    .await;

    // Prospective takers with a bond in flight must not keep waiting on an
    // order that will never be taken. A lookup failure is logged, not
    // propagated: the release below still runs.
    match bond::db::find_active_bonds_for_order(pool, order.id).await {
        Ok(active_bonds) => {
            for active in active_bonds.iter() {
                if let Ok(taker_pk) = PublicKey::from_str(&active.pubkey) {
                    if taker_pk != maker {
                        enqueue_order_msg(
                            None,
                            Some(order.id),
                            Action::AdminCanceled,
                            None,
                            taker_pk,
                            None,
                        )
                        .await;
                    }
                }
            }
        }
        Err(err) => {
            tracing::warn!(
                order_id = %order.id,
                "admin_cancel_pending: failed to look up active bonds for taker notification: {}",
                err
            );
        }
    }
    bond::release_taker_bonds_for_order_or_warn(pool, order.id, "admin_cancel_pending").await;
    if let Err(e) = bond::resolve_range_maker_bond_at_close(pool, ln_client, order).await {
        tracing::warn!(
            order_id = %order.id,
            "admin_cancel_pending: maker bond close failed: {}", e
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::lightning::LndConnector;
    use sqlx::SqlitePool;
    use std::sync::Arc;

    async fn setup_pool() -> Arc<SqlitePool> {
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        sqlx::migrate!("./migrations")
            .run(pool.as_ref())
            .await
            .unwrap();
        pool
    }

    fn build_ctx(pool: Arc<SqlitePool>) -> AppContext {
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .build()
    }

    /// Real `LndConnector` against a dead endpoint: `connect` is lazy so it
    /// always builds; every RPC fails fast. Required because the handler
    /// takes `&mut LndConnector` even on paths that return before any LND
    /// call.
    async fn dead_lnd() -> LndConnector {
        let dir = std::env::temp_dir().join(format!("mostro-test-lnd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("tls.cert");
        let mac = dir.join("admin.macaroon");
        std::fs::write(&cert, b"").unwrap();
        std::fs::write(&mac, [1u8, 2u8]).unwrap();
        let client = fedimint_tonic_lnd::connect(
            "https://127.0.0.1:1".to_string(),
            cert.to_str().unwrap().to_string(),
            mac.to_str().unwrap().to_string(),
        )
        .await
        .expect("lazy connect never dials");
        LndConnector { client }
    }

    fn admin_event(identity: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::new_order(None, Some(1), None, Action::AdminCancel, None),
            signature: None,
            sender: Keys::generate().public_key(),
            identity,
            created_at: Timestamp::now(),
        }
    }

    fn dispute_order(seller: PublicKey, buyer: PublicKey) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::Dispute.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: seller.to_string(),
            seller_pubkey: Some(seller.to_string()),
            buyer_pubkey: Some(buyer.to_string()),
            amount: 21_000,
            fee: 210,
            ..Default::default()
        }
    }

    async fn assign_solver(pool: &SqlitePool, order_id: uuid::Uuid, solver: &PublicKey) {
        let mut dispute = Dispute::new(order_id, Status::Dispute.to_string());
        dispute.status = DisputeStatus::InProgress.to_string();
        dispute.solver_pubkey = Some(solver.to_string());
        dispute.create(pool).await.unwrap();
    }

    fn cancel_msg(order_id: uuid::Uuid) -> Message {
        Message::new_order(Some(order_id), Some(1), None, Action::AdminCancel, None)
    }

    async fn queued_actions_for(destination: PublicKey) -> Vec<Action> {
        crate::config::MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(_, pk)| *pk == destination)
            .map(|(m, _)| m.get_inner_message_kind().action.clone())
            .collect()
    }

    #[tokio::test]
    async fn fails_when_order_missing() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool);
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();

        let result = admin_cancel_action(
            &ctx,
            cancel_msg(uuid::Uuid::new_v4()),
            &admin_event(admin.public_key()),
            &admin,
            &mut ln,
        )
        .await;

        assert!(matches!(result, Err(MostroCantDo(CantDoReason::NotFound))));
    }

    #[tokio::test]
    async fn rejects_caller_not_assigned_and_no_admin_takeover() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let order = dispute_order(seller, buyer)
            .create(ctx.pool())
            .await
            .unwrap();

        let result = admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(Keys::generate().public_key()),
            &admin,
            &mut ln,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::IsNotYourDispute))
        ));
    }

    #[tokio::test]
    async fn reports_admin_takeover_when_dispute_in_progress_with_admin() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let order = dispute_order(seller, buyer)
            .create(ctx.pool())
            .await
            .unwrap();
        assign_solver(ctx.pool(), order.id, &admin.public_key()).await;

        let result = admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(Keys::generate().public_key()),
            &admin,
            &mut ln,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::DisputeTakenByAdmin))
        ));
    }

    #[tokio::test]
    async fn rejects_assigned_solver_without_write_permission() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();
        let solver = Keys::generate();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let order = dispute_order(seller, buyer)
            .create(ctx.pool())
            .await
            .unwrap();
        assign_solver(ctx.pool(), order.id, &solver.public_key()).await;

        let result = admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(solver.public_key()),
            &admin,
            &mut ln,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::NotAuthorized))
        ));
    }

    #[tokio::test]
    async fn cooperatively_cancelled_order_acknowledges_admin() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let mut order = dispute_order(seller, buyer);
        order.status = Status::CooperativelyCanceled.to_string();
        let order = order.create(ctx.pool()).await.unwrap();
        assign_solver(ctx.pool(), order.id, &admin.public_key()).await;

        let result = admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(admin.public_key()),
            &admin,
            &mut ln,
        )
        .await;

        assert!(
            result.is_ok(),
            "coop-cancel must ack and return Ok: {result:?}"
        );
        assert!(queued_actions_for(admin.public_key())
            .await
            .contains(&Action::CooperativeCancelAccepted));
    }

    #[tokio::test]
    async fn rejects_order_not_in_dispute() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let mut order = dispute_order(seller, buyer);
        order.status = Status::Active.to_string();
        let order = order.create(ctx.pool()).await.unwrap();
        assign_solver(ctx.pool(), order.id, &admin.public_key()).await;

        let result = admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(admin.public_key()),
            &admin,
            &mut ln,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::NotAllowedByStatus))
        ));
    }

    /// A dispute order with no `hash` skips the LND cancel and, when
    /// neither `seller_dispute` nor `buyer_dispute` is set, fails the
    /// dispute-initiator resolution with `DisputeEventError`.
    #[tokio::test]
    async fn dispute_without_initiator_flag_errors() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        // hash is None, seller_dispute/buyer_dispute both false.
        let order = dispute_order(seller, buyer)
            .create(ctx.pool())
            .await
            .unwrap();
        assign_solver(ctx.pool(), order.id, &admin.public_key()).await;

        let result = admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(admin.public_key()),
            &admin,
            &mut ln,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::DisputeEventError))
        ));
    }

    /// A dispute order carrying a hold-invoice `hash` returns funds to the
    /// seller via `cancel_hold_invoice`, which fails against the dead LND
    /// endpoint and surfaces as `LnNodeError`.
    #[tokio::test]
    async fn dispute_with_hash_reaches_ln_cancel_seam() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let mut order = dispute_order(seller, buyer);
        order.seller_dispute = true;
        // Valid 32-byte hex hash so `cancel_hold_invoice` reaches the RPC
        // (it panics on non-hex input) and then fails on the dead node.
        order.hash = Some("11".repeat(32));
        let order = order.create(ctx.pool()).await.unwrap();
        assign_solver(ctx.pool(), order.id, &admin.public_key()).await;

        let result = admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(admin.public_key()),
            &admin,
            &mut ln,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::LnNodeError(_)))
        ));
    }

    /// Full no-LND cancel path: a seller-initiated dispute with no hold
    /// invoice hash. The dispute row is moved to `SellerRefunded` and the
    /// order to `CanceledByAdmin` before the DM fan-out. Those DB writes are
    /// deterministic; the terminal `send_dm` depends on the process-global
    /// Nostr client, so the top-level result is not asserted.
    #[tokio::test]
    async fn seller_dispute_without_hash_refunds_and_cancels() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let mut order = dispute_order(seller, buyer);
        order.seller_dispute = true;
        let order = order.create(ctx.pool()).await.unwrap();
        assign_solver(ctx.pool(), order.id, &admin.public_key()).await;
        let dispute_id = find_dispute_by_order_id(ctx.pool(), order.id)
            .await
            .unwrap()
            .id;

        let _ = admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(admin.public_key()),
            &admin,
            &mut ln,
        )
        .await;

        let stored_order = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(stored_order.status, Status::CanceledByAdmin.to_string());
        let stored_dispute = Dispute::by_id(ctx.pool(), dispute_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored_dispute.status,
            DisputeStatus::SellerRefunded.to_string()
        );
    }

    // ── Operator cancel of a still-Pending order ─────────────────────────

    fn pending_sell_order(maker: PublicKey) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::Pending.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: maker.to_string(),
            seller_pubkey: Some(maker.to_string()),
            buyer_pubkey: None,
            amount: 21_000,
            fee: 210,
            ..Default::default()
        }
    }

    /// The daemon key (what the gRPC `CancelOrder` path synthesises as
    /// `identity`) may cancel a `Pending` order: no dispute, no escrow,
    /// no counterparty. The row flips to `CanceledByAdmin` and the maker
    /// is told via `AdminCanceled`.
    #[tokio::test]
    async fn daemon_key_cancels_pending_order_and_notifies_maker() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let daemon = Keys::generate();
        let maker = Keys::generate().public_key();

        let order = pending_sell_order(maker).create(ctx.pool()).await.unwrap();

        admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(daemon.public_key()),
            &daemon,
            &mut ln,
        )
        .await
        .expect("operator cancel of a pending order succeeds");

        let stored = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(stored.status, Status::CanceledByAdmin.to_string());
        assert!(
            queued_actions_for(maker)
                .await
                .contains(&Action::AdminCanceled),
            "maker must be told their order was cancelled by the operator"
        );
    }

    /// A solver (any identity other than the daemon key) has no business
    /// cancelling a Pending order: there is no dispute to be assigned to.
    /// Refused with `NotAuthorized`, and the row is untouched.
    #[tokio::test]
    async fn pending_cancel_refuses_non_daemon_identity() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let daemon = Keys::generate();
        let solver = Keys::generate();
        let maker = Keys::generate().public_key();

        let order = pending_sell_order(maker).create(ctx.pool()).await.unwrap();

        let result = admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(solver.public_key()),
            &daemon,
            &mut ln,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::NotAuthorized))
        ));
        let stored = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(stored.status, Status::Pending.to_string());
        assert!(queued_actions_for(maker).await.is_empty());
    }

    /// A prospective taker who has a bond in flight on the Pending order
    /// is released and notified, so no HTLC is left waiting on an order
    /// that will never be taken.
    #[tokio::test]
    async fn pending_cancel_releases_and_notifies_bonded_taker() {
        use crate::app::bond::{db::create_bond, model::Bond, BondRole, BondState};

        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let daemon = Keys::generate();
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let order = pending_sell_order(maker).create(ctx.pool()).await.unwrap();
        let bond = create_bond(
            ctx.pool(),
            Bond {
                id: uuid::Uuid::new_v4(),
                order_id: order.id,
                pubkey: taker.to_string(),
                role: BondRole::Taker.to_string(),
                amount_sats: 1_000,
                state: BondState::Requested.to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(daemon.public_key()),
            &daemon,
            &mut ln,
        )
        .await
        .expect("operator cancel of a pending order succeeds");

        let stored_bond = crate::app::bond::db::find_bond_by_id(ctx.pool(), bond.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored_bond.state, BondState::Released.to_string());
        assert!(queued_actions_for(taker)
            .await
            .contains(&Action::AdminCanceled));
    }

    /// `WaitingTakerBond` is the same pre-trade window as `Pending` (a
    /// taker is mid-bond, still no escrow): the operator can cancel it too,
    /// and the in-flight taker bond is released.
    #[tokio::test]
    async fn daemon_key_cancels_waiting_taker_bond_order_and_releases_bond() {
        use crate::app::bond::{db::create_bond, model::Bond, BondRole, BondState};

        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let daemon = Keys::generate();
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = pending_sell_order(maker);
        order.status = Status::WaitingTakerBond.to_string();
        let order = order.create(ctx.pool()).await.unwrap();
        let bond = create_bond(
            ctx.pool(),
            Bond {
                id: uuid::Uuid::new_v4(),
                order_id: order.id,
                pubkey: taker.to_string(),
                role: BondRole::Taker.to_string(),
                amount_sats: 1_000,
                state: BondState::Locked.to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        admin_cancel_action(
            &ctx,
            cancel_msg(order.id),
            &admin_event(daemon.public_key()),
            &daemon,
            &mut ln,
        )
        .await
        .expect("operator cancel during the taker bond window succeeds");

        let stored = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(stored.status, Status::CanceledByAdmin.to_string());
        let stored_bond = crate::app::bond::db::find_bond_by_id(ctx.pool(), bond.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored_bond.state, BondState::Released.to_string());
        assert!(queued_actions_for(taker)
            .await
            .contains(&Action::AdminCanceled));
    }

    /// A take that commits between the operator's read and the CAS wins:
    /// the cancel reports `NotAllowedByStatus`, the escrowed row keeps its
    /// status and nobody is told the order was cancelled.
    #[tokio::test]
    async fn pending_cancel_loses_cas_to_a_concurrent_take() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let daemon = Keys::generate();
        let maker = Keys::generate().public_key();

        // The operator's snapshot says Pending, but the row has already
        // moved on to `waiting-payment` (a take committed).
        let snapshot = pending_sell_order(maker).create(ctx.pool()).await.unwrap();
        let mut taken = snapshot.clone();
        taken.status = Status::WaitingPayment.to_string();
        taken.update(ctx.pool()).await.unwrap();

        let result = admin_cancel_pending_order(ctx.pool(), &snapshot, &daemon, &mut ln).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::NotAllowedByStatus))
        ));
        let stored = Order::by_id(ctx.pool(), snapshot.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, Status::WaitingPayment.to_string());
        assert!(queued_actions_for(maker).await.is_empty());
    }

    // ── tolerate_gone_hold_invoice ───────────────────────────────────────

    fn ln_err(msg: &str) -> MostroError {
        MostroInternalErr(ServiceError::LnNodeError(msg.to_string()))
    }

    /// An escrow whose HTLC is already gone must not block the dispute
    /// close: LND "not found" / "already canceled" are treated as done.
    #[test]
    fn gone_hold_invoice_is_not_an_error() {
        let id = uuid::Uuid::new_v4();
        assert!(tolerate_gone_hold_invoice(&id, Ok::<(), _>(())).is_ok());
        for msg in [
            "code=NotFound message=unable to locate invoice",
            "code=Unknown message=invoice already canceled",
            "code=AlreadyExists message=duplicate",
        ] {
            assert!(
                tolerate_gone_hold_invoice(&id, Err::<(), _>(ln_err(msg))).is_ok(),
                "{msg} must be tolerated"
            );
        }
    }

    /// Anything that may leave the HTLC encumbered still aborts the cancel.
    #[test]
    fn transient_ln_failure_still_aborts() {
        let id = uuid::Uuid::new_v4();
        for msg in [
            "code=Unavailable message=transport error",
            "code=DeadlineExceeded message=timeout",
            "code=Internal message=something broke",
        ] {
            assert!(
                matches!(
                    tolerate_gone_hold_invoice(&id, Err::<(), _>(ln_err(msg))),
                    Err(MostroInternalErr(ServiceError::LnNodeError(_)))
                ),
                "{msg} must propagate"
            );
        }
    }
}
