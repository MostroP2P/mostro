use crate::util::{enqueue_order_msg, get_order};

use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use mostro_core::message::{Action, Message};
use mostro_core::order::Status;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

pub async fn trade_pubkey_action(
    msg: Message,
    event: &UnwrappedGift,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let mut order = get_order(&msg, pool).await?;

    // Check if the order status is pending
    if let Err(cause) = order.check_status(Status::Pending) {
        return Err(MostroCantDo(cause));
    }

    match (
        order.master_buyer_pubkey.as_ref(),
        order.master_seller_pubkey.as_ref(),
    ) {
        (Some(master_buyer_pubkey), _) if master_buyer_pubkey == &event.sender.to_string() => {
            order.buyer_pubkey = Some(event.rumor.pubkey.to_string());
        }
        (_, Some(master_seller_pubkey)) if master_seller_pubkey == &event.sender.to_string() => {
            order.seller_pubkey = Some(event.rumor.pubkey.to_string());
        }
        _ => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };
    order.creator_pubkey = event.rumor.pubkey.to_string();

    // We a message to the seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::TradePubkey,
        None,
        event.rumor.pubkey,
        None,
    )
    .await;

    order
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}
