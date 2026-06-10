use crate::app::bond::{self, BondSlashReason};
use crate::app::context::AppContext;
use crate::db::{
    ensure_dispute_finalize_permission, find_dispute_by_order_id, is_assigned_solver,
    is_dispute_taken_by_admin,
};
use crate::lightning::LndConnector;
use crate::nip33::{create_platform_tag_values, new_dispute_event};
use crate::util::{enqueue_order_msg, get_order, settle_seller_hold_invoice, update_order_event};

use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::error;

use super::release::do_payment;

pub async fn admin_settle_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
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
    settle_seller_hold_invoice(event, ctx, Action::AdminSettled, true, &order)
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
    // The anti-abuse bond is Lightning-only (mutually exclusive with Cashu
    // mode), so settling slashed bonds always goes through an LND connector.
    // The escrow seam is not involved here — bond hold invoices are distinct
    // from the trade escrow.
    let mut ln_client = LndConnector::new()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?;
    if let Err(e) = bond::apply_bond_resolution(
        pool,
        &mut ln_client,
        &order_updated,
        &bond_resolution,
        BondSlashReason::LostDispute,
    )
    .await
    {
        tracing::warn!(
            order_id = %order_updated.id,
            "admin_settle: bond resolution apply failed: {}", e
        );
    }

    let _ = do_payment(ctx, order_updated, request_id).await;

    Ok(())
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
