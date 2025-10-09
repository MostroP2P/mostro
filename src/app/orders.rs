use mostro_core::prelude::*;
use crate::util::{enqueue_order_msg, get_user_orders_by_id};
// use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
// use sqlx_crud::Crud;

// Handle orders action
pub async fn orders_action(
    msg: Message,
    event: &UnwrappedGift,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get order
    let payload = msg.get_inner_message_kind().get_payload();

    let ids = match payload {
        Some(Payload::Ids(ids)) => ids,
        _ => return Err(MostroCantDo(CantDoReason::NotFound)), // Add InvalidPayload
    };
    tracing::info!("ids: {:#?}", ids);
    
    // Return empty vector if no valid UUIDs
    if ids.is_empty() {
        return Err(MostroCantDo(CantDoReason::NotFound));
    }
    // Get orders
    let orders = get_user_orders_by_id(pool, ids, &event.rumor.pubkey.to_string()).await?;
    if orders.is_empty() {
        return Err(MostroCantDo(CantDoReason::NotFound));
    }
    let small_orders = orders.iter().map(|order| SmallOrder::from(order.clone())).collect::<Vec<SmallOrder>>();
    let response_payload = Payload::Orders(small_orders);
    tracing::info!("response_payload: {:#?}", response_payload);
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        None,
        Action::Orders,
        Some(response_payload),
        event.rumor.pubkey,
        None,
    )
    .await;

    Ok(())
}
