use crate::config::settings::Settings;
use crate::util::{enqueue_order_msg, get_user_orders_by_id};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

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
        _ => return Err(MostroCantDo(CantDoReason::InvalidParameters)),
    };

    // Return an error to the caller if the payload contains no usable identifiers
    if ids.is_empty() {
        return Err(MostroCantDo(CantDoReason::InvalidParameters));
    }

    let mostro_settings = Settings::get_mostro();
    if ids.len() > mostro_settings.max_orders_per_response as usize {
        return Err(MostroCantDo(CantDoReason::TooManyRequests));
    }

    // Get orders
    let orders = get_user_orders_by_id(pool, ids, &event.sender.to_string()).await?;
    if orders.is_empty() {
        return Err(MostroCantDo(CantDoReason::NotFound));
    }
    let requester = event.sender.to_string();
    let small_orders = orders
        .into_iter()
        .map(|order| {
            let is_buyer = order.master_buyer_pubkey.as_deref() == Some(requester.as_str());
            let mut small = SmallOrder::from(order);
            // Clear buyer_invoice if requester is not the buyer
            if !is_buyer {
                small.buyer_invoice = None;
            }
            small
        })
        .collect::<Vec<SmallOrder>>();
    let response_payload = Payload::Orders(small_orders);
    // Enqueue response message
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
