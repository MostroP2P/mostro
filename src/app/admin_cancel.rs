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
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;
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

    if order.hash.is_some() {
        // We return funds to seller
        if let Some(hash) = order.hash.as_ref() {
            ln_client.cancel_hold_invoice(hash).await?;
            info!("Order Id {}: Funds returned to seller", &order.id);
        }
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
    if let Err(e) = bond::apply_bond_resolution(
        pool,
        ln_client,
        &order,
        &bond_resolution,
        BondSlashReason::LostDispute,
    )
    .await
    {
        tracing::warn!(
            order_id = %order.id,
            "admin_cancel: bond resolution apply failed: {}", e
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use mostro_core::error::CantDoReason;
    use mostro_core::prelude::MostroError;

    const DAEMON_PUBKEY: &str = "b1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
    const READ_ONLY_SOLVER: &str =
        "c1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
    const READ_WRITE_SOLVER: &str =
        "d1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";

    async fn setup_permission_db() -> sqlx::SqlitePool {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();
        sqlx::query(include_str!("../../migrations/20221222153301_orders.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../../migrations/20251126120000_dev_fee.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../../migrations/20231005195154_users.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../../migrations/20230928145530_disputes.sql"))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    async fn seed_solver(pool: &sqlx::SqlitePool, pubkey: &str, category: i64) {
        sqlx::query(
            "INSERT INTO users (pubkey, is_solver, category, created_at) VALUES (?1, 1, ?2, 1700000000)",
        )
        .bind(pubkey)
        .bind(category)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_dispute(pool: &sqlx::SqlitePool, order_id: uuid::Uuid, solver_pubkey: &str) {
        sqlx::query(
            "INSERT INTO disputes (id, order_id, status, order_previous_status, solver_pubkey, created_at)
             VALUES (?1, ?2, 'in-progress', 'dispute', ?3, 1700000000)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(order_id)
        .bind(solver_pubkey)
        .execute(pool)
        .await
        .unwrap();
    }

    /// admin_cancel: read-only solver is rejected with NotAuthorized
    #[tokio::test]
    async fn admin_cancel_read_only_solver_is_rejected() {
        let pool = setup_permission_db().await;
        let order_id = uuid::Uuid::new_v4();
        seed_solver(&pool, READ_ONLY_SOLVER, 1).await;
        seed_dispute(&pool, order_id, READ_ONLY_SOLVER).await;

        let result = crate::db::ensure_dispute_finalize_permission(
            &pool,
            READ_ONLY_SOLVER,
            DAEMON_PUBKEY,
            order_id,
        )
        .await;

        assert!(
            matches!(
                result,
                Err(MostroError::MostroCantDo(CantDoReason::NotAuthorized))
            ),
            "read-only solver must be rejected with NotAuthorized, got: {:?}",
            result
        );
    }

    /// admin_cancel: read-write solver is allowed through the permission gate
    #[tokio::test]
    async fn admin_cancel_read_write_solver_is_allowed() {
        let pool = setup_permission_db().await;
        let order_id = uuid::Uuid::new_v4();
        seed_solver(&pool, READ_WRITE_SOLVER, 2).await;
        seed_dispute(&pool, order_id, READ_WRITE_SOLVER).await;

        let result = crate::db::ensure_dispute_finalize_permission(
            &pool,
            READ_WRITE_SOLVER,
            DAEMON_PUBKEY,
            order_id,
        )
        .await;

        assert!(
            result.is_ok(),
            "read-write solver must pass the permission gate, got: {:?}",
            result
        );
    }
}
