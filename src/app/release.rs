use crate::cli::settings::Settings;
use crate::db::{self};
use crate::lightning::LndConnector;
use crate::lnurl::resolv_ln_address;
use crate::util::{
    get_keys, rate_counterpart, send_cant_do_msg, send_new_order_msg, settle_seller_hold_invoice,
    update_order_event,
};
use crate::NOSTR_CLIENT;
use anyhow::{Error, Result};
use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;
use lnurl::lightning_address::LightningAddress;
use mostro_core::message::{Action, CantDoReason, Message, Payload};
use mostro_core::order::{Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::cmp::Ordering;
use std::str::FromStr;
use tokio::sync::mpsc::channel;
use tracing::{error, info};

pub async fn check_failure_retries(order: &Order, request_id: Option<u64>) -> Result<Order> {
    let mut order = order.clone();

    // Handle to db here
    let pool = db::connect().await?;

    // Get max number of retries
    let ln_settings = Settings::get_ln();
    let retries_number = ln_settings.payment_attempts as i64;

    // Mark payment as failed
    if !order.failed_payment {
        order.failed_payment = true;
        order.payment_attempts = 0;
    } else if order.payment_attempts < retries_number {
        order.payment_attempts += 1;
    }
    let buyer_pubkey = match &order.buyer_pubkey {
        Some(buyer) => PublicKey::from_str(buyer.as_str())?,
        None => return Err(Error::msg("Missing buyer pubkey")),
    };

    send_new_order_msg(
        request_id,
        Some(order.id),
        Action::PaymentFailed,
        None,
        &buyer_pubkey,
        None,
    )
    .await;

    // Update order
    let result = order.update(&pool).await?;
    Ok(result)
}

pub async fn release_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    let next_trade: Option<(String, u32)> = match &msg.get_inner_message_kind().payload {
        Some(Payload::NextTrade(pubkey, index)) => Some((pubkey.clone(), *index)),
        _ => None,
    };

    let order_id = msg
        .get_inner_message_kind()
        .id
        .ok_or(Error::msg("No order id"))?;

    let mut order = Order::by_id(pool, order_id)
        .await?
        .ok_or(Error::msg("Order not found"))?;

    let seller_pubkey_hex = order
        .seller_pubkey
        .as_ref()
        .ok_or(Error::msg("No seller pubkey found"))?;

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

    if &event.rumor.pubkey.to_string() != seller_pubkey_hex {
        send_cant_do_msg(
            request_id,
            Some(order.id),
            Some(CantDoReason::InvalidPeer),
            &event.rumor.pubkey,
        )
        .await;
        return Ok(());
    }

    settle_seller_hold_invoice(
        event,
        ln_client,
        Action::Released,
        false,
        &order,
        request_id,
    )
    .await?;

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
    if let Ok((true, mut child_order)) = get_child_order(&mut order, request_id, my_keys).await {
        handle_child_order(&mut child_order, &order, next_trade, pool, request_id).await?;
    }

    // We send a HoldInvoicePaymentSettled message to seller, the client should
    // indicate *funds released* message to seller
    send_new_order_msg(
        request_id,
        Some(order_id),
        Action::HoldInvoicePaymentSettled,
        None,
        &event.rumor.pubkey,
        None,
    )
    .await;

    // Finally we try to pay buyer's invoice
    let _ = do_payment(order, request_id).await;

    Ok(())
}

// Helper function to manage child order creation for range orders
async fn handle_child_order(
    child_order: &mut Order,
    order: &Order,
    next_trade: Option<(String, u32)>,
    pool: &Pool<Sqlite>,
    request_id: Option<u64>,
) -> Result<()> {
    if let Some((next_trade_pubkey, next_trade_index)) = next_trade {
        if &order.creator_pubkey == order.seller_pubkey.as_ref().unwrap() {
            child_order.seller_pubkey = Some(next_trade_pubkey.clone());
            child_order.creator_pubkey = next_trade_pubkey.clone();
            child_order.trade_index_seller = Some(next_trade_index as i64);
        } else if &order.creator_pubkey == order.buyer_pubkey.as_ref().unwrap() {
            child_order.buyer_pubkey = Some(next_trade_pubkey.clone());
            child_order.creator_pubkey = next_trade_pubkey.clone();
            child_order.trade_index_buyer = order.next_trade_index;
        }

        let new_order = child_order.as_new_order();
        let next_trade_pubkey = PublicKey::from_str(&next_trade_pubkey)?;
        send_new_order_msg(
            request_id,
            new_order.id,
            Action::NewOrder,
            Some(Payload::Order(new_order)),
            &next_trade_pubkey,
            child_order
                .trade_index_buyer
                .or(child_order.trade_index_seller),
        )
        .await;
        child_order.clone().update(pool).await?;
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

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())?,
            PublicKey::from_str(buyer.as_str())?,
        ),
        (None, _) => return Err(Error::msg("Missing seller pubkey")),
        (_, None) => return Err(Error::msg("Missing buyer pubkey")),
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
                                &buyer_pubkey,
                                &seller_pubkey,
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
    buyer_pubkey: &PublicKey,
    seller_pubkey: &PublicKey,
    my_keys: &Keys,
    request_id: Option<u64>,
) -> Result<()> {
    // Purchase completed message to buyer
    send_new_order_msg(
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
        if let Ok(order_success) = order_updated.update(&pool).await {
            // Adding here rate process
            rate_counterpart(buyer_pubkey, seller_pubkey, &order_success, request_id).await?;
        }
    }
    Ok(())
}

/// Check if order is range type
/// Add parent range id and update max amount
/// publish a new replaceable kind nostr event with the status updated
/// and update on local database the status and new event id
pub async fn get_child_order(
    order: &mut Order,
    request_id: Option<u64>,
    my_keys: &Keys,
) -> Result<(bool, Order)> {
    let (Some(max_amount), Some(min_amount)) = (order.max_amount, order.min_amount) else {
        return Ok((false, order.clone()));
    };

    if let Some(new_max) = max_amount.checked_sub(order.fiat_amount) {
        let mut new_order = create_base_order(order);

        match new_max.cmp(&min_amount) {
            Ordering::Equal => {
                update_order_for_equal(new_max, &mut new_order, my_keys).await?;
                return Ok((true, new_order));
            }
            Ordering::Greater => {
                update_order_for_greater(new_max, &mut new_order, my_keys).await?;
                return Ok((true, new_order));
            }
            Ordering::Less => {
                notify_invalid_amount(order, request_id).await;
                return Ok((false, order.clone()));
            }
        }
    }

    Ok((false, order.clone()))
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

async fn update_order_for_equal(new_max: i64, new_order: &mut Order, my_keys: &Keys) -> Result<()> {
    let pool = db::connect().await?;
    new_order.fiat_amount = new_max;
    new_order.max_amount = None;
    new_order.min_amount = None;

    let tags = crate::nip33::order_to_tags(new_order, None);
    let event = crate::nip33::new_event(my_keys, "", new_order.id.to_string(), tags)?;
    new_order.event_id = event.id.to_string();
    new_order.clone().create(&pool).await?;
    NOSTR_CLIENT
        .get()
        .ok_or_else(|| anyhow::Error::msg("NOSTR_CLIENT not initialized"))?
        .send_event(event)
        .await
        .map_err(|err| anyhow::Error::msg(err.to_string()))?;

    Ok(())
}

async fn update_order_for_greater(
    new_max: i64,
    new_order: &mut Order,
    my_keys: &Keys,
) -> Result<()> {
    let pool = db::connect().await?;
    new_order.max_amount = Some(new_max);
    new_order.fiat_amount = 0;

    let tags = crate::nip33::order_to_tags(new_order, None);
    let event = crate::nip33::new_event(my_keys, "", new_order.id.to_string(), tags)?;
    new_order.event_id = event.id.to_string();
    new_order.clone().create(&pool).await?;
    NOSTR_CLIENT
        .get()
        .ok_or_else(|| anyhow::Error::msg("NOSTR_CLIENT not initialized"))?
        .send_event(event)
        .await
        .map_err(|err| anyhow::Error::msg(err.to_string()))?;

    Ok(())
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

        send_cant_do_msg(
            None,
            Some(order.id),
            Some(CantDoReason::InvalidAmount),
            &buyer_pubkey,
        )
        .await;

        send_cant_do_msg(
            request_id,
            Some(order.id),
            Some(CantDoReason::InvalidAmount),
            &seller_pubkey,
        )
        .await;
    }
}
