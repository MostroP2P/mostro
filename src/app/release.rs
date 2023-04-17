use crate::db::{self};
use crate::lightning::LndConnector;
use crate::messages;
use crate::util::{connect_nostr, get_keys};
use crate::util::{send_dm, update_order_event};

use anyhow::Result;
use log::{error, info};
use mostro_core::order::Order;
use mostro_core::{Action, Content, Message, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tokio::sync::mpsc::channel;
use tonic_openssl_lnd::lnrpc::payment::PaymentStatus;

#[tokio::main]
pub async fn release_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    let order_id = msg.order_id.unwrap();
    let order = match Order::by_id(&pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Release: Order Id {order_id} not found!");
            return Ok(());
        }
    };
    let seller_pubkey = event.pubkey;
    if Some(seller_pubkey.to_bech32()?) != order.seller_pubkey {
        let text_message = messages::cant_do();
        // We create a Message
        let message = Message::new(
            0,
            Some(order.id),
            Action::CantDo,
            Some(Content::TextMessage(text_message)),
        );
        let message = message.as_json()?;
        send_dm(&client, &my_keys, &event.pubkey, message).await?;
    }

    if order.preimage.is_none() {
        return Ok(());
    }
    let preimage = order.preimage.as_ref().unwrap();
    ln_client.settle_hold_invoice(preimage).await?;
    info!("Release: Order Id {}: Released sats", &order.id);
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    update_order_event(
        &pool,
        &client,
        &my_keys,
        Status::SettledHoldInvoice,
        &order,
        None,
    )
    .await?;

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
                            "Release: Order Id {}: Invoice with hash: {} paid!",
                            order.id, msg.payment.payment_hash
                        );
                        // Purchase completed message to buyer
                        let message =
                            Message::new(0, Some(order.id), Action::PurchaseCompleted, None);
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
                    }
                }
            }
        }
    };
    tokio::spawn(payment);
    Ok(())
}
