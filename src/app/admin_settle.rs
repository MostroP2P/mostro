use crate::app::bond::{self, BondSlashReason};
use crate::app::context::AppContext;
use crate::db::{
    ensure_dispute_finalize_permission, find_dispute_by_order_id, is_assigned_solver,
    is_dispute_taken_by_admin,
};
use crate::lightning::LndConnector;
use crate::nip33::{create_platform_tag_values, new_dispute_event};
use crate::util::{enqueue_order_msg, get_order, settle_seller_hold_invoice, update_order_event};

use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use std::str::FromStr;
use tracing::error;

use super::release::do_payment;

pub async fn admin_settle_action(
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

    match is_assigned_solver(pool, &event.identity.to_string(), order.id).await {
        Ok(false) => {
            // Check if admin has taken over the dispute
            if is_dispute_taken_by_admin(pool, order.id, &my_keys.public_key().to_string()).await? {
                return Err(MostroCantDo(
                    mostro_core::error::CantDoReason::DisputeTakenByAdmin,
                ));
            } else {
                return Err(MostroCantDo(
                    mostro_core::error::CantDoReason::IsNotYourDispute,
                ));
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

    if let Err(cause) = order.check_status(Status::Dispute) {
        return Err(MostroCantDo(cause));
    }

    // Phase 2: extract and validate the optional `BondResolution` payload
    // here — after the status guards above (which are non-destructive
    // early returns, so an admin retry against an already-cooperatively-
    // cancelled or out-of-dispute order still gets the prior status-
    // driven response) and before any trade-side mutation
    // (`settle_seller_hold_invoice` / `update_order_event` below). On a
    // `slash_*=true` for a side with no `Locked` bond row we return
    // `CantDo(InvalidPayload)` and the trade does not settle; the solver
    // resends a corrected directive. Absent payload ≡
    // `BondResolution { false, false }` ≡ Phase 1 behaviour (release all
    // active bonds, slash none). See `docs/ANTI_ABUSE_BOND.md` §7.3.
    let bond_resolution = bond::extract_bond_resolution(&msg);
    bond::validate_bond_resolution(pool, &order, &bond_resolution).await?;

    // Settle seller hold invoice
    settle_seller_hold_invoice(event, ln_client, Action::AdminSettled, true, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?;
    // Update order event
    let order_updated = update_order_event(my_keys, Status::SettledHoldInvoice, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Persist the status change to DB before calling do_payment (same reason as release_action)
    let result =
        sqlx::query("UPDATE orders SET status = ?, event_id = ? WHERE id = ? AND status = ?")
            .bind(&order_updated.status)
            .bind(&order_updated.event_id)
            .bind(order_updated.id)
            .bind(Status::Dispute.to_string())
            .execute(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if result.rows_affected() == 0 {
        tracing::warn!(
            "Order {} not transitioned to settled-hold-invoice: status changed concurrently",
            order_updated.id
        );
        return Ok(());
    }

    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order.id).await;

    if let Ok(mut d) = dispute {
        let dispute_id = d.id;
        // we update the dispute
        d.status = DisputeStatus::Settled.to_string();
        d.update(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        // Get the creator of the dispute
        let dispute_initiator = match (order.seller_dispute, order.buyer_dispute) {
            (true, false) => "seller",
            (false, true) => "buyer",
            (_, _) => return Err(MostroInternalErr(ServiceError::DisputeEventError)),
        };

        // We create a tag to show status of the dispute
        let tags: Tags = Tags::from_list(vec![
            Tag::custom(
                TagKind::Custom(std::borrow::Cow::Borrowed("s")),
                vec![DisputeStatus::Settled.to_string()],
            ),
            // Who is the dispute creator
            Tag::custom(
                TagKind::Custom(std::borrow::Cow::Borrowed("initiator")),
                vec![dispute_initiator],
            ),
            Tag::custom(
                TagKind::Custom(std::borrow::Cow::Borrowed("y")),
                create_platform_tag_values(ctx.settings().mostro.name.as_deref()),
            ),
            Tag::custom(
                TagKind::Custom(std::borrow::Cow::Borrowed("z")),
                vec!["dispute".to_string()],
            ),
        ]);

        // nip33 kind with dispute id as identifier (kind 38386 for disputes)
        let event = new_dispute_event(my_keys, "", dispute_id.to_string(), tags)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

        // Print event dispute with update
        tracing::info!("Dispute event to be published: {event:#?}");

        let client = ctx.nostr_client();
        if let Err(e) = client.send_event(&event).await {
            error!("Failed to send dispute settlement event: {}", e);
        }
    }

    // Send message to event creator
    enqueue_order_msg(
        request_id,
        Some(order_updated.id),
        Action::AdminSettled,
        None,
        event.sender,
        msg.get_inner_message_kind().trade_index,
    )
    .await;

    // Send message to seller and buyer
    if let Some(ref seller_pubkey) = order_updated.seller_pubkey {
        enqueue_order_msg(
            None,
            Some(order_updated.id),
            Action::AdminSettled,
            None,
            PublicKey::from_str(seller_pubkey)
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            msg.get_inner_message_kind().trade_index,
        )
        .await;
    }
    // Send message to buyer
    if let Some(ref buyer_pubkey) = order_updated.buyer_pubkey {
        enqueue_order_msg(
            None,
            Some(order_updated.id),
            Action::AdminSettled,
            None,
            PublicKey::from_str(buyer_pubkey)
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            msg.get_inner_message_kind().trade_index,
        )
        .await;
    }
    // Phase 2: apply the solver's `BondResolution` (release-by-default
    // when absent, otherwise slash the flagged sides). Slashed bonds
    // have their hold invoices settled immediately; the recipient
    // payout (asking the winning counterparty for a bolt11,
    // `send_payment`, retries, forfeiture on the long-stop window) is
    // still Phase 3's job.
    // #768: notify each slashed party with a best-effort `BondSlashed`
    // forfeiture notice, mirroring the timeout-slash path. Only confirmed
    // slashes are returned, so a dropped settle never produces an untruthful
    // notice and an idempotent retry never re-notifies.
    match bond::apply_bond_resolution(
        pool,
        ln_client,
        &order_updated,
        &bond_resolution,
        BondSlashReason::LostDispute,
    )
    .await
    {
        Ok(slashed_rows) => {
            for slashed in &slashed_rows {
                bond::notify_bond_slashed(&order_updated, slashed).await;
            }
        }
        Err(e) => {
            tracing::warn!(
                order_id = %order_updated.id,
                "admin_settle: bond resolution apply failed: {}", e
            );
        }
    }

    // Phase 6: a dispute resolution ends the range (no remainder is
    // republished), so resolve the maker bond at close — settle the parent
    // HTLC once and refund the unslashed remainder if any slice was slashed,
    // otherwise release. A no-op for non-range maker bonds (already handled
    // inline by `apply_bond_resolution`) and for orders with no maker bond.
    if let Err(e) = bond::resolve_range_maker_bond_at_close(pool, ln_client, &order_updated).await {
        tracing::warn!(
            order_id = %order_updated.id,
            "admin_settle: maker bond close failed: {}", e
        );
    }

    let _ = do_payment(ctx, order_updated, request_id).await;

    Ok(())
}

#[cfg(test)]
mod handler_tests {
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
    /// always builds, but every RPC fails fast. Handlers take `&mut
    /// LndConnector` by value, so tests must supply one even for paths that
    /// return before any LND call.
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

    /// `event.identity` is the seal signer the admin-gating checks against;
    /// `sender` is the trade key.
    fn admin_event(identity: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::new_order(None, Some(1), None, Action::AdminSettle, None),
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
        // `Dispute::new` always starts in `Initiated`; the admin-takeover
        // detection queries for `in-progress`, so set it explicitly.
        let mut dispute = Dispute::new(order_id, Status::Dispute.to_string());
        dispute.status = DisputeStatus::InProgress.to_string();
        dispute.solver_pubkey = Some(solver.to_string());
        dispute.create(pool).await.unwrap();
    }

    fn settle_msg(order_id: uuid::Uuid) -> Message {
        Message::new_order(Some(order_id), Some(1), None, Action::AdminSettle, None)
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

        let result = admin_settle_action(
            &ctx,
            settle_msg(uuid::Uuid::new_v4()),
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

        // No dispute row → not assigned and not taken by admin.
        let result = admin_settle_action(
            &ctx,
            settle_msg(order.id),
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
        // Dispute is in-progress and taken over by the admin (mostro) key.
        assign_solver(ctx.pool(), order.id, &admin.public_key()).await;

        // Caller is some unrelated solver key, not assigned.
        let result = admin_settle_action(
            &ctx,
            settle_msg(order.id),
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

    /// Caller is the assigned solver but is neither the admin key nor a
    /// read-write solver user row → `ensure_dispute_finalize_permission`
    /// rejects with `NotAuthorized`.
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

        let result = admin_settle_action(
            &ctx,
            settle_msg(order.id),
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

    /// A cooperatively-cancelled order short-circuits: the admin (whose
    /// identity == the mostro key, so the finalize-permission admin
    /// shortcut applies) is acknowledged and the handler returns `Ok`
    /// before any settle.
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
        // Admin identity is the assigned solver → finalize permission via
        // the caller==admin shortcut.
        assign_solver(ctx.pool(), order.id, &admin.public_key()).await;

        let result = admin_settle_action(
            &ctx,
            settle_msg(order.id),
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

    /// An order that is neither cooperatively-cancelled nor in dispute is
    /// rejected by the status guard.
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

        let result = admin_settle_action(
            &ctx,
            settle_msg(order.id),
            &admin_event(admin.public_key()),
            &admin,
            &mut ln,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidOrderStatus))
        ));
    }

    /// A genuine dispute settle reaches `settle_seller_hold_invoice`, which
    /// short-circuits on the missing preimage before any LND call and is
    /// mapped to `LnNodeError`. The LND settle + `do_payment` tail beyond
    /// this seam requires a live node and is covered by integration tests.
    #[tokio::test]
    async fn dispute_order_reaches_settle_seam() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let mut ln = dead_lnd().await;
        let admin = Keys::generate();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let mut order = dispute_order(seller, buyer);
        order.seller_dispute = true;
        order.preimage = None; // settle short-circuits with InvalidInvoice
        let order = order.create(ctx.pool()).await.unwrap();
        assign_solver(ctx.pool(), order.id, &admin.public_key()).await;

        let result = admin_settle_action(
            &ctx,
            settle_msg(order.id),
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
}

#[cfg(test)]
mod tests {
    use mostro_core::error::CantDoReason;

    /// Test that our error handling logic correctly identifies admin takeover vs regular disputes
    /// This tests the core business logic of issue #302 without complex database setup
    #[test]
    fn test_dispute_error_types() {
        // Test that we have the correct error types available
        // This ensures our mostro-core dependency includes the new DisputeTakenByAdmin variant

        // Original error for regular dispute issues
        let regular_error = CantDoReason::IsNotYourDispute;
        assert_eq!(format!("{:?}", regular_error), "IsNotYourDispute");

        // New error for admin takeover scenarios
        let admin_error = CantDoReason::DisputeTakenByAdmin;
        assert_eq!(format!("{:?}", admin_error), "DisputeTakenByAdmin");

        // New error for authenticated callers lacking enough permissions
        let unauthorized_error = CantDoReason::NotAuthorized;
        assert_eq!(format!("{:?}", unauthorized_error), "NotAuthorized");

        // Verify they are different error types
        assert_ne!(regular_error, admin_error);
        assert_ne!(admin_error, unauthorized_error);
    }
}
