use crate::cli::settings::Settings;
use crate::db::{self};
use crate::lightning::LndConnector;
use crate::lnurl::resolv_ln_address;
use crate::util::{
    enqueue_cant_do_msg, enqueue_order_msg, get_keys, get_order, rate_counterpart,
    settle_seller_hold_invoice, update_order_event,
};
use anyhow::{Error, Result};
use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;
use lnurl::lightning_address::LightningAddress;
use mostro_core::error::{CantDoReason, MostroError, MostroError::*, ServiceError};
use mostro_core::message::{Action, Message, Payload};
use mostro_core::order::{Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::cmp::Ordering;
use std::str::FromStr;
use tokio::sync::mpsc::channel;
use tracing::{error, info};

/// Check if order has failed payment retries
pub async fn check_failure_retries(
    order: &Order,
    request_id: Option<u64>,
) -> Result<Order, MostroError> {
    let mut order = order.clone();

    // Handle to db here
    let pool = db::connect()
        .await
        .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;

    // Get max number of retries
    let ln_settings = Settings::get_ln();
    let retries_number = ln_settings.payment_attempts as i64;
    // Count payment retries up to limit
    order.count_failed_payment(retries_number);

    let buyer_pubkey = order
        .get_buyer_pubkey()
        .map_err(|cause| MostroInternalErr(cause))?;

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
    let order = get_order(&msg, pool).await?;
    // Get seller pubkey hex
    let seller_pubkey = order
        .get_seller_pubkey()
        .map_err(|cause| MostroInternalErr(cause))
        .and_then(|pk| {
            if pk.to_string() == event.rumor.pubkey.to_string() {
                Ok(pk)
            } else {
                Err(MostroCantDo(CantDoReason::InvalidPeer))
            }
        });

    // Check if order is active, fiat sent or dispute
    if order.check_status(Status::Active).is_err()
        && order.check_status(Status::FiatSent).is_err()
        && order.check_status(Status::Dispute).is_err()
    {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    let next_trade: Option<(String, u32)> = match event.rumor.pubkey.to_string() {
        pubkey if pubkey == order.creator_pubkey => {
            if let Some(Payload::NextTrade(pubkey, index)) = &msg.get_inner_message_kind().payload {
                Some((pubkey.clone(), *index))
            } else {
                None
            }
        }
        _ => match (order.next_trade_pubkey.as_ref(), order.next_trade_index) {
            (Some(pubkey), Some(index)) => Some((pubkey.clone(), index as u32)),
            _ => None,
        },
    };

    let current_status =
        Status::from_str(&order.status).map_err(|_| Error::msg("Wrong order status"))?;

    if !matches!(
        current_status,
        Status::Active | Status::FiatSent | Status::Dispute
    ) {
        send_cant_do_msg(
            request_id,
            Some(order.id),
            Some(CantDoReason::NotAllowedByStatus),
            &event.rumor.pubkey,
        )
        .await;
        return Ok(());
    }

    settle_seller_hold_invoice(event, ln_client, Action::Released, false, &order).await?;

    // We send a message to buyer indicating seller released funds
    let buyer_pubkey = PublicKey::from_str(
        order
            .buyer_pubkey
            .as_ref()
            .ok_or(Error::msg("Missing buyer pubkey"))?
            .as_str(),
    )?;

    send_new_order_msg(
        None,
        Some(order_id),
        Action::Released,
        None,
        &buyer_pubkey,
        None,
    )
    .await;
    order = update_order_event(my_keys, Status::SettledHoldInvoice, &order).await?;
    // Handle child order for range orders
    if let Ok((Some(child_order), Some(event))) =
        get_child_order(order.clone(), request_id, my_keys).await
    {
        if let Ok(client) = get_nostr_client() {
            if client.send_event(event).await.is_err() {
                tracing::warn!("Failed sending child order event for order id: {}. This may affect order synchronization", child_order.id)
            }
        }
        handle_child_order(child_order, &order, next_trade, pool, request_id).await?;
    }

    // We send a HoldInvoicePaymentSettled message to seller, the client should
    // indicate *funds released* message to seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::HoldInvoicePaymentSettled,
        None,
        seller_pubkey?,
        None,
    )
    .await;

    // We send a message to buyer indicating seller released funds
    let buyer_pubkey = order
        .get_buyer_pubkey()
        .map_err(|cause| MostroInternalErr(cause))?;
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::Released,
        None,
        buyer_pubkey,
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
    child_order: Order,
    order: &Order,
    next_trade: Option<(String, u32)>,
    pool: &Pool<Sqlite>,
    request_id: Option<u64>,
) -> Result<()> {
    if let Some((next_trade_pubkey, next_trade_index)) = next_trade {
        let mut child_order = child_order;
        if &order.creator_pubkey == order.seller_pubkey.as_ref().unwrap() {
            child_order.seller_pubkey = Some(next_trade_pubkey.clone());
            child_order.creator_pubkey = next_trade_pubkey.clone();
            child_order.trade_index_seller = Some(next_trade_index as i64);
        } else if &order.creator_pubkey == order.buyer_pubkey.as_ref().unwrap() {
            child_order.buyer_pubkey = Some(next_trade_pubkey.clone());
            child_order.creator_pubkey = next_trade_pubkey.clone();
            child_order.trade_index_buyer = order.next_trade_index;
        }
        child_order.next_trade_index = None;
        child_order.next_trade_pubkey = None;

        let new_order = child_order.as_new_order();
        let next_trade_pubkey = PublicKey::from_str(&next_trade_pubkey)?;
        send_new_order_msg(
            request_id,
            new_order.id,
            Action::NewOrder,
            Some(Payload::Order(new_order)),
            &next_trade_pubkey,
            Some(next_trade_index as i64),
        )
        .await;
        child_order.create(pool).await?;
    }
    Ok(())
}

pub async fn do_payment(mut order: Order, request_id: Option<u64>) -> Result<()> {
    let payment_request = match order.buyer_invoice.as_ref() {
        Some(req) => req.to_string(),
        _ => return Err(Error::msg("Missing payment request")),
    };

    let ln_addr = LightningAddress::from_str(&payment_request);
    let amount = order.amount as u64 - order.fee as u64;
    let payment_request = if let Ok(addr) = ln_addr {
        resolv_ln_address(&addr.to_string(), amount).await?
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

    let my_keys = get_keys()?;

    let buyer_pubkey = match &order.buyer_pubkey {
        Some(buyer) => PublicKey::from_str(buyer.as_str())?,
        None => return Err(Error::msg("Missing buyer pubkey")),
    };

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
                            let _ = payment_success(
                                &mut order,
                                buyer_pubkey,
                                seller_pubkey,
                                &my_keys,
                                request_id,
                            )
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
    seller_pubkey: PublicKey,
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

    if let Ok(order_updated) = update_order_event(my_keys, Status::Success, order).await {
        let pool = db::connect().await?;
        if let Ok(order) = order_updated.update(&pool).await {
            // Send dm to buyer to rate counterpart
            send_new_order_msg(
                request_id,
                Some(order.id),
                Action::Rate,
                None,
                buyer_pubkey,
                None,
            )
            .await;
        }
    }
    Ok(())
}

/// Check if order is range type
/// Add parent range id and update max amount
/// publish a new replaceable kind nostr event with the status updated
/// and update on local database the status and new event id
pub async fn get_child_order(
    order: Order,
    request_id: Option<u64>,
    my_keys: &Keys,
) -> Result<(Option<Order>, Option<Event>)> {
    let (Some(max_amount), Some(min_amount)) = (order.max_amount, order.min_amount) else {
        return Ok((None, None));
    };

    if let Some(new_max) = max_amount.checked_sub(order.fiat_amount) {
        let mut new_order = create_base_order(&order);

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
                notify_invalid_amount(&order, request_id).await;
                return Ok((None, None));
            }
        }
    }

    Ok((None, None))
}

fn create_base_order(order: &Order) -> Order {
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

    if new_order.kind == "sell" {
        new_order.buyer_pubkey = None;
        new_order.master_buyer_pubkey = None;
        new_order.trade_index_buyer = None;
    } else {
        new_order.seller_pubkey = None;
        new_order.master_seller_pubkey = None;
        new_order.trade_index_seller = None;
    }

    new_order
}

fn create_order_event(new_order: &mut Order, my_keys: &Keys) -> Result<Event> {
    let tags = crate::nip33::order_to_tags(new_order, None);
    let event =
        crate::nip33::new_event(my_keys, "", new_order.id.to_string(), tags).map_err(|e| {
            tracing::error!("Failed to create event for order {}: {}", new_order.id, e);
            e
        })?;
    new_order.event_id = event.id.to_string();
    Ok(event)
}

async fn order_for_equal(
    new_max: i64,
    new_order: &mut Order,
    my_keys: &Keys,
) -> Result<(Order, Event)> {
    new_order.fiat_amount = new_max;
    new_order.max_amount = None;
    new_order.min_amount = None;
    let event = create_order_event(new_order, my_keys)?;

    Ok((new_order.clone(), event))
}

async fn order_for_greater(
    new_max: i64,
    new_order: &mut Order,
    my_keys: &Keys,
) -> Result<(Order, Event)> {
    new_order.max_amount = Some(new_max);
    new_order.fiat_amount = 0;
    let event = create_order_event(new_order, my_keys)?;

    Ok((new_order.clone(), event))
}

async fn notify_invalid_amount(order: &Order, request_id: Option<u64>) {
    if let (Some(buyer_pubkey), Some(seller_pubkey)) =
        (order.buyer_pubkey.as_ref(), order.seller_pubkey.as_ref())
    {
        let buyer_pubkey = match PublicKey::from_str(buyer_pubkey) {
            Ok(pk) => pk,
            Err(e) => {
                error!("Failed to parse buyer pubkey: {:?}", e);
                return;
            }
        };
        let seller_pubkey = match PublicKey::from_str(seller_pubkey) {
            Ok(pk) => pk,
            Err(e) => {
                error!("Failed to parse seller pubkey: {:?}", e);
                return;
            }
        };

        enqueue_cant_do_msg(
            None,
            Some(order.id),
            CantDoReason::InvalidAmount,
            buyer_pubkey,
        )
        .await;

        enqueue_cant_do_msg(
            request_id,
            Some(order.id),
            CantDoReason::InvalidAmount,
            seller_pubkey,
        )
        .await;
    }
}
