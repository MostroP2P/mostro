use crate::app::context::AppContext;
use crate::util::{enqueue_order_msg, get_order};

use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;

pub async fn trade_pubkey_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let mut order = get_order(&msg, pool).await?;

    // Check if the order status is pending
    if let Err(cause) = order.check_status(Status::Pending) {
        return Err(MostroCantDo(cause));
    }

    // Get master keys (already plaintext after phase-3/4 encryption migration)
    let (master_buyer_key, master_seller_key) = if order.master_buyer_pubkey.is_some() {
        let master_buyer_key = order
            .get_master_buyer_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
        (Some(master_buyer_key), None)
    } else {
        let master_seller_key = order
            .get_master_seller_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
        (None, Some(master_seller_key))
    };

    match (master_buyer_key, master_seller_key) {
        (Some(master_buyer_pubkey), _) if master_buyer_pubkey == event.identity.to_string() => {
            order.buyer_pubkey = Some(event.sender.to_string());
        }
        (_, Some(master_seller_pubkey)) if master_seller_pubkey == event.identity.to_string() => {
            order.seller_pubkey = Some(event.sender.to_string());
        }
        _ => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };
    order.creator_pubkey = event.sender.to_string();

    // We a message to the seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::TradePubkey,
        None,
        event.sender,
        None,
    )
    .await;

    order
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}
