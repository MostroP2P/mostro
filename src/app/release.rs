use crate::cli::settings::Settings;
use crate::db;
use crate::lightning::LndConnector;
use crate::lnurl::resolv_ln_address;
use crate::util::{
    connect_nostr, get_keys, rate_counterpart, send_dm, settle_seller_hold_invoice,
    update_order_event,
};

use anyhow::Result;
use lnurl::lightning_address::LightningAddress;
use mostro_core::message::{Action, Content, Message};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tokio::sync::mpsc::channel;
use tonic_openssl_lnd::lnrpc::payment::PaymentStatus;
use tracing::{error, info};

pub async fn check_failure_retries(order: &Order) -> Result<Order> {
    let mut order = order.clone();

    // Handle to db here
    let pool = db::connect().await.unwrap();

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

    // Update order
    let result = order.update(&pool).await?;
    Ok(result)
}

pub async fn release_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    let order_id = msg.get_inner_message_kind().id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };
    let seller_pubkey_hex = match order.seller_pubkey {
        Some(ref pk) => pk,
        None => {
            error!("Order Id {}: Seller pubkey not found!", order.id);
            return Ok(());
        }
    };
    let seller_pubkey = event.pubkey;

    let current_status = Status::from_str(&order.status).unwrap();
    if current_status != Status::Active
        && current_status != Status::FiatSent
        && current_status != Status::Dispute
    {
        let message = Message::cant_do(Some(order.id), None, None);
        send_dm(client, my_keys, &event.pubkey, message.as_json()?).await?;

        return Ok(());
    }

    if &seller_pubkey.to_string() != seller_pubkey_hex {
        let message = Message::cant_do(
            Some(order.id),
            None,
            Some(Content::TextMessage(
                "You are not allowed to release funds for this order!".to_string(),
            )),
        );
        send_dm(client, my_keys, &event.pubkey, message.as_json()?).await?;

        return Ok(());
    }

    settle_seller_hold_invoice(
        event,
        my_keys,
        client,
        ln_client,
        Action::Release,
        false,
        &order,
    )
    .await?;

    let buyer_pubkey = order.buyer_pubkey.clone().unwrap();

    let order_updated =
        update_order_event(client, my_keys, Status::SettledHoldInvoice, &order).await?;

    // We send a HoldInvoicePaymentSettled message to seller, the client should
    // indicate *funds released* message to seller
    let message = Message::new_order(
        Some(order_id),
        None,
        Action::HoldInvoicePaymentSettled,
        None,
    );

    send_dm(client, my_keys, &seller_pubkey, message.as_json()?).await?;
    // We send a message to buyer indicating seller released funds
    let message = Message::new_order(Some(order_id), None, Action::Release, None);
    let message = message.as_json()?;
    let buyer_pubkey = XOnlyPublicKey::from_str(&buyer_pubkey)?;
    send_dm(client, my_keys, &buyer_pubkey, message).await?;
    let _ = do_payment(order_updated).await;

    Ok(())
}

pub async fn do_payment(order: Order) -> Result<()> {
    // Finally we try to pay buyer's invoice
    let payment_request = order.buyer_invoice.as_ref().unwrap().to_string();
    let ln_addr = LightningAddress::from_str(&payment_request);
    let amount = order.amount as u64 - order.fee as u64;
    let payment_request = if let Ok(addr) = ln_addr {
        resolv_ln_address(&addr.to_string(), amount).await?
    } else {
        payment_request
    };
    let mut ln_client_payment = LndConnector::new().await;
    let (tx, mut rx) = channel(100);

    let payment_task = ln_client_payment.send_payment(&payment_request, amount as i64, tx);
    if let Err(paymement_result) = payment_task.await {
        info!("Error during ln payment : {}", paymement_result);
        if let Ok(failed_payment) = check_failure_retries(&order).await {
            info!(
                "Order id {} has {} failed payments retries",
                failed_payment.id, failed_payment.payment_attempts
            );
        }
    }

    let payment = {
        async move {
            // We redeclare vars to use inside this block
            let client = connect_nostr().await.unwrap();
            let my_keys = get_keys().unwrap();
            let buyer_pubkey =
                XOnlyPublicKey::from_str(order.buyer_pubkey.as_ref().unwrap()).unwrap();
            let seller_pubkey =
                XOnlyPublicKey::from_str(order.seller_pubkey.as_ref().unwrap()).unwrap();
            // Receiving msgs from send_payment()
            while let Some(msg) = rx.recv().await {
                if let Some(status) = PaymentStatus::from_i32(msg.payment.status) {
                    match status {
                        PaymentStatus::Succeeded => {
                            info!(
                                "Order Id {}: Invoice with hash: {} paid!",
                                order.id, msg.payment.payment_hash
                            );
                            payment_success(
                                &order,
                                &buyer_pubkey,
                                &seller_pubkey,
                                &my_keys,
                                &client,
                            )
                            .await;
                        }
                        PaymentStatus::Failed => {
                            info!(
                                "Order Id {}: Invoice with hash: {} has failed!",
                                order.id, msg.payment.payment_hash
                            );

                            // Mark payment as failed
                            if let Ok(failed_payment) = check_failure_retries(&order).await {
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
    order: &Order,
    buyer_pubkey: &XOnlyPublicKey,
    seller_pubkey: &XOnlyPublicKey,
    my_keys: &Keys,
    client: &Client,
) {
    // Purchase completed message to buyer
    let message = Message::new_order(Some(order.id), None, Action::PurchaseCompleted, None);
    let message = message.as_json().unwrap();
    send_dm(client, my_keys, buyer_pubkey, message)
        .await
        .unwrap();

    // Let's wait 5 secs before publish this new event
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    if let Ok(order_updated) = update_order_event(client, my_keys, Status::Success, order).await {
        let pool = db::connect().await.unwrap();
        if let Ok(order_success) = order_updated.update(&pool).await {
            // Adding here rate process
            rate_counterpart(client, buyer_pubkey, seller_pubkey, my_keys, &order_success)
                .await
                .unwrap();
        }
    }
}
