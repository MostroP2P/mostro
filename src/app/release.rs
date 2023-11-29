use crate::db::{self};
use crate::lightning::LndConnector;
use crate::util::{connect_nostr, get_keys, settle_seller_hold_invoice};
use crate::util::{rate_counterpart, send_dm, update_order_event};

use anyhow::Result;
use log::{error, info};
use mostro_core::message::{Action, Message};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tokio::sync::mpsc::channel;
use tonic_openssl_lnd::lnrpc::payment::PaymentStatus;

pub async fn release_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    let order_id = msg.order_id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };
    let seller_pubkey = event.pubkey;
    let status = Status::SettledHoldInvoice;
    let action = Action::Release;

    settle_seller_hold_invoice(
        event, my_keys, client, pool, ln_client, status, action, false, &order,
    )
    .await?;

    // We send a HoldInvoicePaymentSettled message to seller, the client should
    // indicate *funds released* message to seller
    let message = Message::new(
        0,
        Some(order.id),
        None,
        Action::HoldInvoicePaymentSettled,
        None,
    );
    let message = message.as_json()?;
    send_dm(client, my_keys, &seller_pubkey, message).await?;
    // We send a message to buyer indicating seller released funds
    let message = Message::new(0, Some(order.id), None, Action::Release, None);
    let message = message.as_json()?;
    let buyer_pubkey = XOnlyPublicKey::from_bech32(order.buyer_pubkey.as_ref().unwrap())?;
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
                XOnlyPublicKey::from_bech32(order.buyer_pubkey.as_ref().unwrap()).unwrap();
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
                        let message =
                            Message::new(0, Some(order.id), None, Action::PurchaseCompleted, None);
                        let message = message.as_json().unwrap();
                        send_dm(&client, &my_keys, &buyer_pubkey, message)
                            .await
                            .unwrap();
                        let status = Status::Success;
                        // We publish a new replaceable kind nostr event with the status updated
                        // and update on local database the status and new event id
                        update_order_event(&pool, &client, &my_keys, status, &order, None)
                            .await
                            .unwrap();

                        // Adding here rate process
                        rate_counterpart(
                            &client,
                            &buyer_pubkey,
                            &seller_pubkey,
                            &my_keys,
                            order.as_new_order().clone(),
                        )
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
