use crate::config;
use crate::config::MOSTRO_DB_PASSWORD;
use crate::db::{self};
use crate::lightning::LndConnector;
use crate::lnurl::resolv_ln_address;
use crate::nip33::{new_event, order_to_tags};
use crate::util::{
    enqueue_order_msg, get_keys, get_nostr_client, get_order, settle_seller_hold_invoice,
    update_order_event,
};

use config::settings::*;
use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;
use lnurl::lightning_address::LightningAddress;
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::cmp::Ordering;
use std::str::FromStr;
use tokio::sync::mpsc::channel;
use tracing::info;

/// Check if order has failed payment retries
pub async fn check_failure_retries(
    order: &Order,
    request_id: Option<u64>,
) -> Result<Order, MostroError> {
    let mut order = order.clone();

    // Arc clone of db pool to use across threads
    let pool = get_db_pool();

    // Get max number of retries
    let ln_settings = Settings::get_ln();
    let retries_number = ln_settings.payment_attempts as i64;
    // Count payment retries up to limit
    order.count_failed_payment(retries_number);

    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::PaymentFailed,
        None,
        buyer_pubkey,
        None,
    )
    .await;

    // Update order
    let result = order
        .update(&pool)
        .await
        .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;
    Ok(result)
}

pub async fn release_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let mut order = get_order(&msg, pool).await?;
    // Get seller pubkey hex
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    // We send a message to buyer indicating seller released funds
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Check if the pubkey is the seller pubkey - Only the seller can release funds
    if seller_pubkey != event.rumor.pubkey {
        return Err(MostroCantDo(CantDoReason::InvalidPeer));
    }

    // Check if order is active, fiat sent or dispute
    if order.check_status(Status::Active).is_err()
        && order.check_status(Status::FiatSent).is_err()
        && order.check_status(Status::Dispute).is_err()
    {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    // Get next trade key
    let next_trade = msg
        .get_inner_message_kind()
        .get_next_trade_key()
        .map_err(MostroInternalErr)?;

    // Settle seller hold invoice
    settle_seller_hold_invoice(event, ln_client, Action::Released, false, &order).await?;
    // Update order event with status SettledHoldInvoice
    order = update_order_event(my_keys, Status::SettledHoldInvoice, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    enqueue_order_msg(
        None,
        Some(order.id),
        Action::Released,
        None,
        buyer_pubkey,
        None,
    )
    .await;

    // Handle child order for range orders
    if let Ok((Some(child_order), Some(event))) = get_child_order(order.clone(), my_keys).await {
        if let Ok(client) = get_nostr_client() {
            if client.send_event(&event).await.is_err() {
                tracing::warn!("Failed sending child order event for order id: {}. This may affect order synchronization", child_order.id)
            }
        }
        handle_child_order(child_order, &order, next_trade, pool, request_id)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    }

    // We send a HoldInvoicePaymentSettled message to seller, the client should
    // indicate *funds released* message to seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::HoldInvoicePaymentSettled,
        None,
        seller_pubkey,
        None,
    )
    .await;

    // We send a message to seller indicating seller released funds
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::Rate,
        None,
        seller_pubkey,
        None,
    )
    .await;

    // Finally we try to pay buyer's invoice
    let _ = do_payment(order, request_id).await;

    Ok(())
}

/// Manages the creation and update of child orders in a range order sequence
///
/// # Arguments
/// * `child_order` - The child order to be created/updated
/// * `order` - The parent order
/// * `next_trade` - Optional tuple of (pubkey, index) for the next trade
/// * `pool` - Database connection pool
/// * `request_id` - Optional request ID for messaging
///
/// # Returns
/// Result indicating success or failure of the operation
async fn handle_child_order(
    mut child_order: Order,
    order: &Order,
    next_trade: Option<(String, u32)>,
    pool: &Pool<Sqlite>,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    // Check if users are in rating mode or full privacy mode - in case get the user id keys for rate
    let (normal_buyer_idkey, normal_seller_idkey) = order
        .is_full_privacy_order(MOSTRO_DB_PASSWORD.get())
        .map_err(|_| {
            MostroInternalErr(ServiceError::UnexpectedError(
                "Error creating order event".to_string(),
            ))
        })?;

    // if it's a buy order use the next trade pubkey and index created when buyer sends fiat to seller
    let (notification_pubkey, new_trade_index) = if order.is_buy_order().is_ok()
        && order.buyer_pubkey.as_ref() == Some(&order.creator_pubkey)
    {
        child_order.buyer_pubkey = order.next_trade_pubkey.clone();
        child_order.trade_index_buyer = order.next_trade_index;
        let next_buyer_pubkey = if let Some(next_trade_pubkey) = order.next_trade_pubkey.clone() {
            next_trade_pubkey
        } else {
            return Err(MostroInternalErr(ServiceError::UnexpectedError(
                "Next trade buyer pubkey is missing".to_string(),
            )));
        };

        child_order.creator_pubkey = next_buyer_pubkey.clone();

        if normal_buyer_idkey.is_none() {
            child_order.master_buyer_pubkey = Some(
                CryptoUtils::store_encrypted(&next_buyer_pubkey, MOSTRO_DB_PASSWORD.get(), None)
                    .map_err(|_| {
                        MostroInternalErr(ServiceError::EncryptionError(
                            "Error storing encrypted master buyer pubkey".to_string(),
                        ))
                    })?,
            );
        }

        // Clear next trade fields - just in case of buy order because they were added when buyer do fiat sent command
        child_order.next_trade_index = None;
        child_order.next_trade_pubkey = None;

        (
            child_order.buyer_pubkey.clone(),
            child_order.trade_index_buyer,
        )
    } else if order.is_sell_order().is_ok()
        && order.seller_pubkey.as_ref() == Some(&order.creator_pubkey)
    {
        if let Some((next_trade_pubkey, next_trade_index)) = next_trade {
            let next_trade_pubkey = PublicKey::from_str(&next_trade_pubkey)
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;
            // Get if users are in full privacy mode to use correct master keys in child order

            child_order.seller_pubkey = Some(next_trade_pubkey.to_string());
            child_order.trade_index_seller = Some(next_trade_index as i64);
            child_order.creator_pubkey = next_trade_pubkey.to_string();

            if normal_seller_idkey.is_none() {
                child_order.master_seller_pubkey = Some(
                    CryptoUtils::store_encrypted(
                        &next_trade_pubkey.to_string(),
                        MOSTRO_DB_PASSWORD.get(),
                        None,
                    )
                    .map_err(|_| {
                        MostroInternalErr(ServiceError::EncryptionError(
                            "Error storing encrypted master seller pubkey".to_string(),
                        ))
                    })?,
                );
            }
        }

        (
            child_order.seller_pubkey.clone(),
            child_order.trade_index_seller,
        )
    } else {
        return Err(MostroInternalErr(ServiceError::UnexpectedError(
            "Next trade seller pubkey is missing".to_string(),
        )));
    };

    // Prepare new pending child order
    let new_order = child_order.as_new_order();

    if let (Some(destination_pubkey), new_trade_index) = (notification_pubkey, new_trade_index) {
        // If we have next trade pubkey and index we can set them in child order
        enqueue_order_msg(
            request_id,
            new_order.id,
            Action::NewOrder,
            Some(Payload::Order(new_order)),
            PublicKey::from_str(&destination_pubkey).map_err(|_| {
                MostroInternalErr(ServiceError::NostrError("Invalid pubkey".to_string()))
            })?,
            new_trade_index,
        )
        .await;
    } else {
        return Err(MostroInternalErr(ServiceError::UnexpectedError(
            "Next trade indecx or pubkey is missing - user cannot be notified".to_string(),
        )));
    }

    // Evertyhing is ok, we can create the child order event
    child_order
        .create(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}

pub async fn do_payment(mut order: Order, request_id: Option<u64>) -> Result<(), MostroError> {
    let payment_request = match order.buyer_invoice.as_ref() {
        Some(req) => req.to_string(),
        _ => return Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
    };

    let ln_addr = LightningAddress::from_str(&payment_request);
    let amount = order.amount as u64 - order.fee as u64;
    let payment_request = if let Ok(addr) = ln_addr {
        resolv_ln_address(&addr.to_string(), amount)
            .await
            .map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?
    } else {
        payment_request
    };
    let mut ln_client_payment = LndConnector::new().await?;
    let (tx, mut rx) = channel(100);

    let payment_task = ln_client_payment.send_payment(&payment_request, amount as i64, tx);
    if let Err(paymement_result) = payment_task.await {
        info!("Error during ln payment : {}", paymement_result);
        if let Ok(failed_payment) = check_failure_retries(&order, request_id).await {
            info!(
                "Order id {} has {} failed payments retries",
                failed_payment.id, failed_payment.payment_attempts
            );
        }
    }

    // Get Mostro keys
    let my_keys =
        get_keys().map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Get buyer and seller pubkeys
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    let payment = {
        async move {
            // We redeclare vars to use inside this block
            // Receiving msgs from send_payment()
            while let Some(msg) = rx.recv().await {
                if let Ok(status) = PaymentStatus::try_from(msg.payment.status) {
                    match status {
                        PaymentStatus::Succeeded => {
                            info!(
                                "Order Id {}: Invoice with hash: {} paid!",
                                order.id, msg.payment.payment_hash
                            );
                            let _ = payment_success(&mut order, buyer_pubkey, &my_keys, request_id)
                                .await;
                        }
                        PaymentStatus::Failed => {
                            info!(
                                "Order Id {}: Invoice with hash: {} has failed!",
                                order.id, msg.payment.payment_hash
                            );

                            // Mark payment as failed
                            if let Ok(failed_payment) =
                                check_failure_retries(&order, request_id).await
                            {
                                info!(
                                    "Order id {} has {} failed payments retries",
                                    failed_payment.id, failed_payment.payment_attempts
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    };
    tokio::spawn(payment);
    Ok(())
}

async fn payment_success(
    order: &mut Order,
    buyer_pubkey: PublicKey,
    my_keys: &Keys,
    request_id: Option<u64>,
) -> Result<()> {
    // Purchase completed message to buyer
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::PurchaseCompleted,
        None,
        buyer_pubkey,
        None,
    )
    .await;

    // Get db connection
    let pool = db::connect()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if let Ok(order_updated) = update_order_event(my_keys, Status::Success, order).await {
        let order = order_updated
            .update(&pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        // Send dm to buyer to rate counterpart
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::Rate,
            None,
            buyer_pubkey,
            None,
        )
        .await;
    }
    Ok(())
}

/// Check if order is range type
/// Add parent range id and update max amount
/// publish a new replaceable kind nostr event with the status updated
/// and update on local database the status and new event id
pub async fn get_child_order(
    order: Order,
    my_keys: &Keys,
) -> Result<(Option<Order>, Option<Event>), MostroError> {
    let (Some(max_amount), Some(min_amount)) = (order.max_amount, order.min_amount) else {
        return Ok((None, None));
    };

    if let Some(new_max) = max_amount.checked_sub(order.fiat_amount) {
        let mut new_order = create_base_order(&order)?;

        match new_max.cmp(&min_amount) {
            Ordering::Equal => {
                let (order, event) = order_for_equal(new_max, &mut new_order, my_keys).await?;
                return Ok((Some(order), Some(event)));
            }
            Ordering::Greater => {
                let (order, event) = order_for_greater(new_max, &mut new_order, my_keys).await?;
                return Ok((Some(order), Some(event)));
            }
            Ordering::Less => {
                return Ok((None, None));
            }
        }
    }

    Ok((None, None))
}

fn create_base_order(order: &Order) -> Result<Order, MostroError> {
    let mut new_order = order.clone();
    new_order.id = uuid::Uuid::new_v4();
    new_order.status = Status::Pending.to_string();
    new_order.amount = 0;
    new_order.hash = None;
    new_order.preimage = None;
    new_order.buyer_invoice = None;
    new_order.taken_at = 0;
    new_order.invoice_held_at = 0;
    new_order.range_parent_id = Some(order.id);

    match new_order.get_order_kind().map_err(MostroInternalErr)? {
        mostro_core::order::Kind::Sell => {
            new_order.buyer_pubkey = None;
            new_order.master_buyer_pubkey = None;
            new_order.trade_index_buyer = None;
        }
        mostro_core::order::Kind::Buy => {
            new_order.seller_pubkey = None;
            new_order.master_seller_pubkey = None;
            new_order.trade_index_seller = None;
        }
    }

    Ok(new_order)
}

async fn create_order_event(new_order: &mut Order, my_keys: &Keys) -> Result<Event, MostroError> {
    // Arc clone db pool to safe use across threads
    let pool = get_db_pool();

    // Extract user for rating tag
    let identity_pubkey = match new_order.is_sell_order() {
        Ok(_) => new_order
            .get_master_seller_pubkey(MOSTRO_DB_PASSWORD.get())
            .map_err(MostroInternalErr)?,
        Err(_) => new_order
            .get_master_buyer_pubkey(MOSTRO_DB_PASSWORD.get())
            .map_err(MostroInternalErr)?,
    };

    // If user has sent the order with his identity key means that he wants to be rate so we can just
    // check if we have identity key in db - if present we have to send reputation tags otherwise no.
    let tags = match crate::db::is_user_present(&pool, identity_pubkey).await {
        Ok(user) => order_to_tags(
            new_order,
            Some((user.total_rating, user.total_reviews, user.created_at)),
        )?,
        Err(_) => order_to_tags(new_order, Some((0.0, 0, 0)))?,
    };

    // Prepare new child order event for sending
    let event = if let Some(tags) = tags {
        new_event(my_keys, "", new_order.id.to_string(), tags)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?
    } else {
        return Err(MostroInternalErr(ServiceError::UnexpectedError(
            "Error creating order event".to_string(),
        )));
    };

    new_order.event_id = event.id.to_string();
    Ok(event)
}

async fn order_for_equal(
    new_max: i64,
    new_order: &mut Order,
    my_keys: &Keys,
) -> Result<(Order, Event), MostroError> {
    new_order.fiat_amount = new_max;
    new_order.max_amount = None;
    new_order.min_amount = None;
    let event = create_order_event(new_order, my_keys).await?;

    Ok((new_order.clone(), event))
}

async fn order_for_greater(
    new_max: i64,
    new_order: &mut Order,
    my_keys: &Keys,
) -> Result<(Order, Event), MostroError> {
    new_order.max_amount = Some(new_max);
    new_order.fiat_amount = 0;
    let event = create_order_event(new_order, my_keys).await?;

    Ok((new_order.clone(), event))
}
