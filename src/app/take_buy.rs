use crate::util::{
    get_fiat_amount_requested, get_market_amount_and_fee, get_order, show_hold_invoice,
};

use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;

use anyhow::Result;
use mostro_core::message::Message;
use mostro_core::order::Status;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

pub async fn take_buy_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Extract order ID from the message, returning an error if not found
    // Safe unwrap as we verified the message
    let mut order = get_order(&msg, pool).await?;

    // Get the request ID from the message
    let request_id = msg.get_inner_message_kind().request_id;

    // Check if the order is a buy order and if its status is active
    if let Err(cause) = order.is_buy_order() {
        return Err(MostroCantDo(cause));
    };
    // Check if the order status is pending
    if let Err(cause) = order.check_status(Status::Pending) {
        return Err(MostroCantDo(cause));
    }

    // Validate that the order was sent from the correct maker
    if let Err(cause) = order.sent_from_maker(event.rumor.pubkey.to_hex()) {
        return Err(MostroCantDo(cause));
    }

    // Get the fiat amount requested by the user for range orders
    if let Some(am) = get_fiat_amount_requested(&order, &msg) {
        order.fiat_amount = am;
    } else {
        return Err(MostroInternalErr(ServiceError::WrongAmountError));
    }

    // If the order amount is zero, calculate the market price in sats
    if order.has_no_amount() {
        match get_market_amount_and_fee(order.fiat_amount, &order.fiat_code, order.premium).await {
            Ok(amount_fees) => {
                order.amount = amount_fees.0;
                order.fee = amount_fees.1
            }
            Err(_) => return Err(MostroInternalErr(ServiceError::WrongAmountError)),
        };
    }

    // Get seller and buyer public keys
    let seller_pubkey = event.rumor.pubkey;
    let buyer_pubkey = order
        .get_buyer_pubkey()
        .map_err(|cause| MostroInternalErr(cause))?;

    // Add seller identity and trade index to the order
    order.master_seller_pubkey = Some(event.sender.to_string());
    order.trade_index_seller = msg.get_inner_message_kind().trade_index;

    // Timestamp the order take time
    order.set_timestamp_now();

    // Show hold invoice and return success or error
    if let Err(cause) = show_hold_invoice(
        my_keys,
        None,
        &buyer_pubkey,
        &seller_pubkey,
        order,
        request_id,
    )
    .await
    {
        return Err(MostroInternalErr(ServiceError::HoldInvoiceError(
            cause.to_string(),
        )));
    }
    Ok(())
}
