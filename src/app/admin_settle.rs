use crate::app::bond;
use crate::app::context::AppContext;
use crate::db::{
    find_dispute_by_order_id, is_assigned_solver, is_dispute_taken_by_admin,
    solver_has_write_permission,
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

    if !solver_has_write_permission(pool, &event.identity.to_string(), order.id).await? {
        return Err(MostroCantDo(CantDoReason::NotAuthorized));
    }

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
    // Phase 2: `admin_settle` means the seller won the dispute. The
    // taker's identity follows from order kind (Sell-order taker = buyer,
    // Buy-order taker = seller); when the taker lost AND the operator
    // enabled `slash_on_lost_dispute`, the active taker bond moves to
    // `pending-payout` for the Phase 3 payout job. Otherwise the bond
    // is released — the Phase 1 behaviour preserved by default.
    bond::apply_taker_dispute_outcome_or_warn(pool, &order_updated, true, "admin_settle").await;

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
