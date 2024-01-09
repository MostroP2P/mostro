use crate::db;
use crate::lightning::LndConnector;
use crate::util::{
    connect_nostr, get_keys, rate_counterpart, send_dm, settle_seller_hold_invoice,
    update_order_event,
};

use anyhow::Result;
use mostro_core::message::{Action, Content, Message};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tokio::sync::mpsc::channel;
use tonic_openssl_lnd::lnrpc::payment::PaymentStatus;
use tracing::{error, info};

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
    let seller_pubkey = event.pubkey.clone();

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

    let status = Status::SettledHoldInvoice;
    let action = Action::Release;

    settle_seller_hold_invoice(
        event, my_keys, client, pool, ln_client, status, action, false, &order,
    )
    .await?;

    // We send a HoldInvoicePaymentSettled message to seller, the client should
    // indicate *funds released* message to seller
    let message = Message::new_order(
        Some(order.id),
        None,
        Action::HoldInvoicePaymentSettled,
        None,
    );

    send_dm(client, my_keys, &seller_pubkey, message.as_json()?).await?;
    // We send a message to buyer indicating seller released funds
    let message = Message::new_order(Some(order.id), None, Action::Release, None);
    let message = message.as_json()?;
    let buyer_pubkey = XOnlyPublicKey::from_str(order.buyer_pubkey.as_ref().unwrap())?;
    send_dm(client, my_keys, &buyer_pubkey, message).await?;

    // Finally we try to pay buyer's invoice
    let payment_request = order.buyer_invoice.as_ref().unwrap().to_string();
    let mut ln_client_payment = LndConnector::new().await;
    let (tx, mut rx) = channel(100);
    let payment_task = {
        async move {
            ln_client_payment
                .send_payment(&payment_request, order.amount, tx)
                .await;
        }
    };
    tokio::spawn(payment_task);
    let payment = {
        async move {
            // We redeclare vars to use inside this block
            let client = connect_nostr().await.unwrap();
            let my_keys = get_keys().unwrap();
            let buyer_pubkey =
                XOnlyPublicKey::from_str(order.buyer_pubkey.as_ref().unwrap()).unwrap();
            let pool = db::connect().await.unwrap();
            // Receiving msgs from send_payment()
            while let Some(msg) = rx.recv().await {
                if let Some(status) = PaymentStatus::from_i32(msg.payment.status) {
                    if status == PaymentStatus::Succeeded {
                        info!(
                            "Order Id {}: Invoice with hash: {} paid!",
                            order.id, msg.payment.payment_hash
                        );
                        // Purchase completed message to buyer
                        let message = Message::new_order(
                            Some(order.id),
                            None,
                            Action::PurchaseCompleted,
                            None,
                        );
                        let message = message.as_json().unwrap();
                        send_dm(&client, &my_keys, &buyer_pubkey, message)
                            .await
                            .unwrap();
                        let status = Status::Success;
                        // Let's wait 5 secs before publish this new event
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        // We publish a new replaceable kind nostr event with the status updated
                        // and update on local database the status and new event id
                        update_order_event(&pool, &client, &my_keys, status, &order)
                            .await
                            .unwrap();

                        // Adding here rate process
                        rate_counterpart(&client, &buyer_pubkey, &seller_pubkey, &my_keys, &order)
                            .await
                            .unwrap();
                    }
                }
            }
        }
    };
    tokio::spawn(payment);
    Ok(())
}
