use crate::error::MostroError;
use crate::util::{get_fiat_amount_requested, get_market_amount_and_fee, show_hold_invoice};
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use mostro_core::error::CantDoReason;

use anyhow::Result;
use mostro_core::message::Message;
use mostro_core::order::{Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

pub async fn take_buy_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Extract order ID from the message, returning an error if not found
    // Safe unwrap as we verified the message
    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(MostroInternalErr(ServiceError::NoAPIResponse))
    };

    // Get the request ID from the message
    let request_id = msg.get_inner_message_kind().request_id;

    // Retrieve the order from the database using the order ID
    let mut order = match Order::by_id(pool, order_id).await {
        Ok(Some(order)) => order,
        Ok(None) => {
            return Err(MostroInternalErr(ServiceError::InvalidOrderId));
        }
        Err(_) => {
            return Err(MostroInternalErr(ServiceError::DbAccessError));
        }
    };

    // Check if the order is a buy order and if its status is active
    if !order.is_buy_order() {
        return Err(MostroCantDo(CantDoReason::InvalidOrderKind));
    };
    // Check if the order status is pending
    if !order.check_status(Status::Pending) {
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

    // Validate that the order was sent from the correct maker
    if !order.sent_from_maker(event.rumor.pubkey.to_hex()) {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // Get the fiat amount requested by the user for range orders
    if let Some(am) = get_fiat_amount_requested(&order, &msg) {
        order.fiat_amount = am;
    } else {
        list
        return Err(MostroError::WrongAmountError);
    }

    // If the order amount is zero, calculate the market price in sats
    if order.amount == 0 {
        if let Ok((new_sats_amount, fee)) =
            get_market_amount_and_fee(order.fiat_amount, &order.fiat_code, order.premium).await
        {
            // Update order with new sats value and fee
            order.amount = new_sats_amount;
            order.fee = fee;
        } else {
            return Err(MostroError::NoAPIResponse);
        }
    }

    // Get seller and buyer public keys
    let seller_pubkey = event.rumor.pubkey;
    let buyer_pubkey = match order.get_buyer_pubkey() {
        Some(pk) => pk,
        None => return Err(MostroError::InvalidPubkey),
    };

    // Add seller identity and trade index to the order
    order.master_seller_pubkey = Some(event.sender.to_string());
    order.trade_index_seller = msg.get_inner_message_kind().trade_index;

    // Timestamp the order take time
    order.taken_at = Timestamp::now().as_u64() as i64;

    // Show hold invoice and return success or error
    if let Ok(()) = show_hold_invoice(
        my_keys,
        None,
        &buyer_pubkey,
        &seller_pubkey,
        order,
        request_id,
    )
    .await
    {
        Ok(())
    } else {
        Err(MostroError::HoldInvoiceError)
    }
}
