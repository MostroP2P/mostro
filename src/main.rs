pub mod db;
pub mod error;
pub mod flow;
pub mod lightning;
pub mod messages;
pub mod models;
pub mod scheduler;
pub mod util;

use dotenvy::dotenv;
use error::MostroError;
use lightning::invoice::is_valid_invoice;
use log::{error, info};

use nostr_sdk::prelude::*;

use mostro_core::order::Order;
use mostro_core::{Action, Message, Status};
use scheduler::start_scheduler;
use sqlx_crud::Crud;
use std::str::FromStr;
use tokio::sync::mpsc::channel;
use tonic_openssl_lnd::lnrpc::payment::PaymentStatus;
use util::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    pretty_env_logger::init();
    // Connect to database
    let pool = db::connect().await?;
    // Connect to relays
    let client = util::connect_nostr().await?;
    let my_keys = util::get_keys()?;

    let subscription = Filter::new()
        .pubkey(my_keys.public_key())
        .since(Timestamp::now());

    client.subscribe(vec![subscription]).await;
    let mut ln_client = lightning::LndConnector::new().await;

    // Start scheduler for tasks
    start_scheduler().await.unwrap().start().await?;

    loop {
        let mut notifications = client.notifications();

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event(_, event) = notification {
                if let Kind::EncryptedDirectMessage = event.kind {
                    let message = decrypt(
                        &my_keys.secret_key().unwrap(),
                        &event.pubkey,
                        &event.content,
                    );
                    if let Ok(m) = message {
                        let message = Message::from_json(&m);
                        if let Ok(msg) = message {
                            if msg.verify() {
                                match msg.action {
                                    Action::Order => {
                                        if let Some(order) = msg.get_order() {
                                            publish_order(
                                                &pool,
                                                &client,
                                                &my_keys,
                                                order,
                                                &event.pubkey.to_bech32()?,
                                            )
                                            .await?
                                        }
                                    }
                                    Action::TakeSell => {
                                        // Safe unwrap as we verified the message
                                        let order_id = msg.order_id.unwrap();
                                        let mut order = match Order::by_id(&pool, order_id).await? {
                                            Some(order) => order,
                                            None => {
                                                error!("TakeSell: Order Id {order_id} not found!");
                                                break;
                                            }
                                        };
                                        if order.kind != "Sell" {
                                            error!("TakeSell: Order Id {order_id} wrong kind");
                                            break;
                                        }
                                        let buyer_pubkey = event.pubkey;
                                        let pr: Option<String>;
                                        // If a buyer sent me a lightning invoice we look on db an order with
                                        // that order id and save the buyer pubkey and invoice fields
                                        if let Some(payment_request) = msg.get_payment_request() {
                                            let order_amount = if order.amount == 0 {
                                                None
                                            } else {
                                                Some(order.amount as u64)
                                            };

                                            // Verify if invoice is valid
                                            match is_valid_invoice(&payment_request, order_amount) {
                                                Ok(_) => {}
                                                Err(e) => match e {
                                                    MostroError::ParsingInvoiceError
                                                    | MostroError::InvoiceExpiredError
                                                    | MostroError::MinExpirationTimeError
                                                    | MostroError::WrongAmountError
                                                    | MostroError::MinAmountError => {
                                                        send_dm(
                                                            &client,
                                                            &my_keys,
                                                            &buyer_pubkey,
                                                            e.to_string(),
                                                        )
                                                        .await?;
                                                        error!("{e}");
                                                        break;
                                                    }
                                                    _ => {}
                                                },
                                            }
                                            pr = Some(payment_request);
                                        } else {
                                            pr = None;
                                        }

                                        let order_status = match Status::from_str(&order.status) {
                                            Ok(s) => s,
                                            Err(e) => {
                                                error!("TakeSell: Order Id {order_id} wrong status: {e:?}");
                                                break;
                                            }
                                        };
                                        // Buyer can take pending orders only
                                        match order_status {
                                            Status::Pending | Status::WaitingBuyerInvoice => {}
                                            _ => {
                                                send_dm(
                                                    &client,
                                                    &my_keys,
                                                    &buyer_pubkey,
                                                    format!(
                                                        "Order Id {order_id} was already taken!"
                                                    ),
                                                )
                                                .await?;
                                                break;
                                            }
                                        }
                                        let seller_pubkey = match order.seller_pubkey.as_ref() {
                                            Some(pk) => XOnlyPublicKey::from_bech32(pk)?,
                                            None => {
                                                error!(
                                                    "TakeSell: Seller pubkey not found for order {}!",
                                                    order.id
                                                );
                                                break;
                                            }
                                        };

                                        // Check market price value in sats - if order was with market price then calculate it and send a DM to buyer
                                        if order.amount == 0 {
                                            order.amount = set_market_order_sats_amount(
                                                &mut order,
                                                buyer_pubkey,
                                                &my_keys,
                                                &pool,
                                                &client,
                                            )
                                            .await?;
                                        } else {
                                            show_hold_invoice(
                                                &pool,
                                                &client,
                                                &my_keys,
                                                pr,
                                                &buyer_pubkey,
                                                &seller_pubkey,
                                                &order,
                                            )
                                            .await?;
                                        }
                                    }
                                    Action::TakeBuy => {
                                        let seller_pubkey = event.pubkey;
                                        // Safe unwrap as we verified the message
                                        let order_id = msg.order_id.unwrap();
                                        let mut order = match Order::by_id(&pool, order_id).await? {
                                            Some(order) => order,
                                            None => {
                                                error!("TakeBuy: Order Id {order_id} not found!");
                                                break;
                                            }
                                        };
                                        if order.kind != "Buy" {
                                            error!("TakeBuy: Order Id {order_id} wrong kind");
                                            break;
                                        }

                                        let order_status = match Status::from_str(&order.status) {
                                            Ok(s) => s,
                                            Err(e) => {
                                                error!("TakeBuy: Order Id {order_id} wrong status: {e:?}");
                                                break;
                                            }
                                        };
                                        // Buyer can take pending orders only
                                        if order_status != Status::Pending {
                                            send_dm(
                                                &client,
                                                &my_keys,
                                                &seller_pubkey,
                                                format!("Order Id {order_id} was already taken!"),
                                            )
                                            .await?;
                                            break;
                                        }
                                        let buyer_pubkey = match order.buyer_pubkey.as_ref() {
                                            Some(pk) => XOnlyPublicKey::from_bech32(pk)?,
                                            None => {
                                                error!(
                                                    "TakeBuy: Buyer pubkey not found for order {}!",
                                                    order.id
                                                );
                                                break;
                                            }
                                        };
                                        // Check market price value in sats - if order was with market price then calculate it and send a DM to buyer
                                        if order.amount == 0 {
                                            order.amount = set_market_order_sats_amount(
                                                &mut order,
                                                buyer_pubkey,
                                                &my_keys,
                                                &pool,
                                                &client,
                                            )
                                            .await?;
                                        }

                                        show_hold_invoice(
                                            &pool,
                                            &client,
                                            &my_keys,
                                            None,
                                            &buyer_pubkey,
                                            &seller_pubkey,
                                            &order,
                                        )
                                        .await?;
                                    }
                                    Action::FiatSent => {
                                        let order_id = msg.order_id.unwrap();
                                        let order = match Order::by_id(&pool, order_id).await? {
                                            Some(order) => order,
                                            None => {
                                                error!("FiatSent: Order Id {order_id} not found!");
                                                break;
                                            }
                                        };
                                        // TODO: send to user a DM with the error
                                        if order.status != "Active" {
                                            error!("FiatSent: Order Id {order_id} wrong status");
                                            break;
                                        }
                                        // Check if the pubkey is the buyer
                                        if Some(event.pubkey.to_bech32()?) != order.buyer_pubkey {
                                            send_dm(
                                                &client,
                                                &my_keys,
                                                &event.pubkey,
                                                messages::cant_do(),
                                            )
                                            .await?;
                                        }

                                        // We publish a new replaceable kind nostr event with the status updated
                                        // and update on local database the status and new event id
                                        update_order_event(
                                            &pool,
                                            &client,
                                            &my_keys,
                                            Status::FiatSent,
                                            &order,
                                        )
                                        .await?;

                                        let seller_pubkey = match order.seller_pubkey.as_ref() {
                                            Some(pk) => XOnlyPublicKey::from_bech32(pk)?,
                                            None => {
                                                error!(
                                                    "Seller pubkey not found for order {}!",
                                                    order.id
                                                );
                                                break;
                                            }
                                        };
                                        // We send a message to seller to release
                                        let message =
                                            messages::buyer_sentfiat(order.id, event.pubkey)?;
                                        send_dm(&client, &my_keys, &seller_pubkey, message).await?;
                                        // We send a message to buyer to wait
                                        let message =
                                            messages::you_sent_fiat(order.id, seller_pubkey)?;
                                        send_dm(&client, &my_keys, &event.pubkey, message).await?;
                                    }
                                    Action::Release => {
                                        let order_id = msg.order_id.unwrap();
                                        let order = match Order::by_id(&pool, order_id).await? {
                                            Some(order) => order,
                                            None => {
                                                error!("Release: Order Id {order_id} not found!");
                                                break;
                                            }
                                        };
                                        let seller_pubkey = event.pubkey;
                                        if Some(seller_pubkey.to_bech32()?) != order.seller_pubkey {
                                            send_dm(
                                                &client,
                                                &my_keys,
                                                &event.pubkey,
                                                messages::cant_do(),
                                            )
                                            .await?;
                                        }

                                        if order.preimage.is_none() {
                                            break;
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
                                        )
                                        .await?;

                                        // Finally we try to pay buyer's invoice
                                        let payment_request =
                                            order.buyer_invoice.as_ref().unwrap().to_string();
                                        let mut ln_client_payment =
                                            lightning::LndConnector::new().await;
                                        let (tx, mut rx) = channel(100);
                                        let payment_task = {
                                            async move {
                                                ln_client_payment
                                                    .send_payment(
                                                        &payment_request,
                                                        order.amount,
                                                        tx,
                                                    )
                                                    .await;
                                            }
                                        };
                                        tokio::spawn(payment_task);
                                        let payment = {
                                            async move {
                                                // We redeclare vars to use inside this block
                                                let client = util::connect_nostr().await.unwrap();
                                                let my_keys = util::get_keys().unwrap();
                                                let buyer_pubkey = XOnlyPublicKey::from_bech32(
                                                    order.buyer_pubkey.as_ref().unwrap(),
                                                )
                                                .unwrap();
                                                let pool = db::connect().await.unwrap();
                                                // Receiving msgs from send_payment()
                                                while let Some(msg) = rx.recv().await {
                                                    if let Some(status) =
                                                        PaymentStatus::from_i32(msg.payment.status)
                                                    {
                                                        if status == PaymentStatus::Succeeded {
                                                            info!(
                                                                "Release: Order Id {}: Invoice with hash: {} paid!",
                                                                order.id,
                                                                msg.payment.payment_hash
                                                            );
                                                            // Purchase completed message to buyer
                                                            let message =
                                                                messages::purchase_completed(
                                                                    order.id,
                                                                    buyer_pubkey,
                                                                )
                                                                .unwrap();
                                                            send_dm(
                                                                &client,
                                                                &my_keys,
                                                                &buyer_pubkey,
                                                                message,
                                                            )
                                                            .await
                                                            .unwrap();
                                                            let status = Status::Success;
                                                            // We publish a new replaceable kind nostr event with the status updated
                                                            // and update on local database the status and new event id
                                                            update_order_event(
                                                                &pool, &client, &my_keys, status,
                                                                &order,
                                                            )
                                                            .await
                                                            .unwrap();
                                                        }
                                                    }
                                                }
                                            }
                                        };
                                        tokio::spawn(payment);
                                    }
                                    Action::Cancel => {
                                        let order_id = msg.order_id.unwrap();
                                        let order = match Order::by_id(&pool, order_id).await? {
                                            Some(order) => order,
                                            None => {
                                                error!("Cancel: Order Id {order_id} not found!");
                                                break;
                                            }
                                        };
                                        // Validates if this user is the order creator
                                        let user_pubkey = event.pubkey.to_bech32()?;
                                        if user_pubkey != order.creator_pubkey {
                                            send_dm(
                                                &client,
                                                &my_keys,
                                                &event.pubkey,
                                                messages::cant_do(),
                                            )
                                            .await?;
                                            break;
                                        }
                                        // We publish a new replaceable kind nostr event with the status updated
                                        // and update on local database the status and new event id
                                        update_order_event(
                                            &pool,
                                            &client,
                                            &my_keys,
                                            Status::Canceled,
                                            &order,
                                        )
                                        .await?;
                                        // We send a message to seller to release
                                        let message = messages::order_canceled(order.id);
                                        send_dm(&client, &my_keys, &event.pubkey, message).await?;
                                    }
                                    Action::PayInvoice => todo!(),
                                }
                            }
                        }
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use mostro_core::order::NewOrder;
    use mostro_core::Message;

    #[test]
    fn test_order_deserialize_serialize() {
        let sample_order = r#"{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","prime":1}"#;
        let order = NewOrder::from_json(sample_order).unwrap();
        let json_order = order.as_json().unwrap();
        assert_eq!(sample_order, json_order);
    }

    #[test]
    fn test_message_deserialize_serialize() {
        let sample_message = r#"{"version":0,"order_id":"7dd204d2-d06c-4406-a3d9-4415f4a8b9c9","action":"TakeSell","content":{"PaymentRequest":[null,"lnbc1..."]}}"#;
        let message = Message::from_json(sample_message).unwrap();
        assert!(message.verify());
        let json_message = message.as_json().unwrap();
        assert_eq!(sample_message, json_message);
    }

    #[test]
    fn test_wrong_message_should_fail() {
        let sample_message = r#"{"version":0,"action":"TakeSell","content":{"Order":{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","prime":1,"payment_request":null,"created_at":1640839235}}}"#;
        let message = Message::from_json(sample_message).unwrap();
        assert!(!message.verify());
    }
}
