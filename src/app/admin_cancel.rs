use std::borrow::Cow;
use std::str::FromStr;

use crate::app::bond::{self, BondSlashReason};
use crate::app::context::AppContext;
use crate::db::{
    ensure_dispute_finalize_permission, find_dispute_by_order_id, is_assigned_solver,
    is_dispute_taken_by_admin,
};
use crate::lightning::LndConnector;
use crate::nip33::{create_platform_tag_values, new_dispute_event};
use crate::util::{enqueue_order_msg, get_order, send_dm, update_order_event};
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use tracing::{error, info};

/// Admin-initiated order cancellation.
///
/// Allows authorized dispute solvers or admins to cancel an order and refund
/// any held Lightning invoice back to the seller.
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

    // Resolve the dispute initiator *before* the escrow is touched (#805).
    // This match rejects orders whose initiator flags are unset or ambiguous,
    // and `cancel_hold_invoice` below is irreversible: running it first meant
    // a rejected request could still refund the seller while leaving the order
    // in `Dispute`, so the LN side and the DB disagreed with no way back.
    // Every check that can reject the call now precedes the refund.
    let dispute_initiator = match (order.seller_dispute, order.buyer_dispute) {
        (true, false) => "seller",
        (false, true) => "buyer",
        (_, _) => return Err(MostroInternalErr(ServiceError::DisputeEventError)),
    };

    if order.hash.is_some() {
        // We return funds to seller
        if let Some(hash) = order.hash.as_ref() {
            ln_client.cancel_hold_invoice(hash).await?;
            info!("Order Id {}: Funds returned to seller", &order.id);
        }
    }

    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order.id).await;

    if let Ok(mut d) = dispute {
        let dispute_id = d.id;
        // we update the dispute
        d.status = DisputeStatus::SellerRefunded.to_string();
        d.update(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        // We create a tag to show status of the dispute
        let tags: Tags = Tags::from_list(vec![
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("s")),
                vec![DisputeStatus::SellerRefunded.to_string()],
            ),
            // Who is the dispute creator
            Tag::custom(
                TagKind::Custom(std::borrow::Cow::Borrowed("initiator")),
                vec![dispute_initiator],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("y")),
                create_platform_tag_values(ctx.settings().mostro.name.as_deref()),
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("z")),
                vec!["dispute".to_string()],
            ),
        ]);
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

    /// Regression for #805: an order carrying a hold-invoice `hash` whose
    /// dispute-initiator flags are unset must fail validation *before* the
    /// irreversible `cancel_hold_invoice`. Reaching the LND seam here would
    /// surface as `LnNodeError` and would mean the seller was already
    /// refunded on a request that goes on to be rejected.
    #[tokio::test]
    async fn dispute_without_initiator_flag_errors_before_refunding() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let mut order = dispute_order(seller, buyer);
        // Hold invoice present, but neither side is flagged as the dispute
        // initiator, so `dispute_initiator` resolution must reject the call.
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

        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::DisputeEventError))
            ),
            "expected the initiator check to reject before the LND cancel, got {result:?}"
        );

        // The order must be left untouched so the solver can retry.
        let stored = get_order(&cancel_msg(order.id), ctx.pool()).await.unwrap();
        assert_eq!(stored.status, Status::Dispute.to_string());
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
}
