pub mod db;
pub mod error;
pub mod flow;
pub mod lightning;
pub mod messages;
pub mod models;
pub mod types;
pub mod util;

use std::str::FromStr;

use dotenvy::dotenv;
use error::MostroError;
use lightning::invoice::is_valid_invoice;
use log::{error, info};
use models::Order;
use nostr_sdk::nostr::hashes::hex::ToHex;
use nostr_sdk::nostr::util::time::timestamp;
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;
use tokio::sync::mpsc::channel;
use tonic_openssl_lnd::lnrpc::{invoice::InvoiceState, payment::PaymentStatus};
use types::Status;
use util::{publish_order, send_dm, update_order_event};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    pretty_env_logger::init();
    // Connect to database
    let pool = db::connect().await?;
    // Connect to relays
    let client = util::connect_nostr().await?;
    let my_keys = util::get_keys()?;

    let subscription = SubscriptionFilter::new()
        .pubkey(my_keys.public_key())
        .since(timestamp());

    client.subscribe(vec![subscription]).await?;
    let mut ln_client = lightning::LndConnector::new().await;

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
                        let message = types::Message::from_json(&m);
                        if let Ok(msg) = message {
                            if msg.verify() {
                                match msg.action {
                                    types::Action::Order => {
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
                                    types::Action::TakeSell => {
                                        // If a buyer sent me a lightning invoice we look on db an order with
                                        // that order id and save the buyer pubkey and invoice fields
                                        if let Some(payment_request) = msg.get_payment_request() {
                                            let buyer_pubkey = event.pubkey;
                                            // Safe unwrap as we verified the message
                                            let order_id = msg.order_id.unwrap();
                                            let order = match Order::by_id(&pool, order_id).await? {
                                                Some(order) => order,
                                                None => {
                                                    error!(
                                                        "TakeSell Error: Order Id {order_id} not found!"
                                                    );
                                                    break;
                                                }
                                            };
                                            // Verify if invoice is valid
                                            match is_valid_invoice(
                                                &payment_request,
                                                Some(order.amount as u64),
                                            ) {
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

                                            let order_status = match Status::from_str(&order.status)
                                            {
                                                Ok(s) => s,
                                                Err(e) => {
                                                    error!("TakeSell Error: Order Id {order_id} wrong status: {e:?}");
                                                    break;
                                                }
                                            };
                                            // Buyer can take pending orders only
                                            if order_status != Status::Pending {
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
                                            // Now we generate the hold invoice that seller should pay
                                            let (invoice_response, preimage, hash) = ln_client
                                                .create_hold_invoice(
                                                    &messages::hold_invoice_description(
                                                        my_keys.public_key(),
                                                        &order.id.to_string(),
                                                        &order.fiat_code,
                                                        &order.fiat_amount.to_string(),
                                                    )?,
                                                    order.amount,
                                                )
                                                .await?;
                                            db::edit_order(
                                                &pool,
                                                &Status::WaitingPayment,
                                                order_id,
                                                &buyer_pubkey,
                                                &payment_request,
                                                &preimage.to_hex(),
                                                &hash.to_hex(),
                                            )
                                            .await?;
                                            // We need to publish a new event with the new status
                                            update_order_event(
                                                &pool,
                                                &client,
                                                &my_keys,
                                                Status::WaitingPayment,
                                                &order,
                                            )
                                            .await?;
                                            let seller_pubkey = match order.seller_pubkey.as_ref() {
                                                Some(pk) => pk,
                                                None => {
                                                    error!(
                                                        "TakeSell Error: Seller pubkey not found for order {}!",
                                                        order.id
                                                    );
                                                    break;
                                                }
                                            };

                                            let message = messages::payment_request(
                                                &order,
                                                &invoice_response.payment_request,
                                            );
                                            // We send the hold invoice to the seller
                                            send_dm(
                                                &client,
                                                &my_keys,
                                                &XOnlyPublicKey::from_bech32(seller_pubkey)?,
                                                message,
                                            )
                                            .await?;
                                            let message =
                                                messages::waiting_seller_to_pay_invoice(order.id);

                                            // We send a message to buyer to know that seller was requested to pay the invoice
                                            send_dm(&client, &my_keys, &buyer_pubkey, message)
                                                .await?;
                                            let mut ln_client_invoices =
                                                lightning::LndConnector::new().await;
                                            let (tx, mut rx) = channel(100);

                                            let invoice_task = {
                                                async move {
                                                    ln_client_invoices
                                                        .subscribe_invoice(hash, tx)
                                                        .await;
                                                }
                                            };
                                            tokio::spawn(invoice_task);
                                            let subs = {
                                                async move {
                                                    // Receiving msgs from the invoice subscription.
                                                    while let Some(msg) = rx.recv().await {
                                                        let hash = msg.hash.to_hex();
                                                        // If this invoice was paid by the seller
                                                        if msg.state == InvoiceState::Accepted {
                                                            flow::hold_invoice_paid(&hash).await;
                                                            println!("Invoice with hash {hash} accepted!");
                                                        } else if msg.state == InvoiceState::Settled
                                                        {
                                                            // If the payment was released by the seller
                                                            println!(
                                                                "Invoice with hash {hash} settled!"
                                                            );
                                                            flow::hold_invoice_settlement(&hash)
                                                                .await;
                                                        } else if msg.state
                                                            == InvoiceState::Canceled
                                                        {
                                                            // If the payment was canceled
                                                            println!("Invoice with hash {hash} canceled!");
                                                            flow::hold_invoice_canceled(&hash)
                                                                .await;
                                                        } else {
                                                            info!("Invoice with hash: {hash} subscribed!");
                                                        }
                                                    }
                                                }
                                            };
                                            tokio::spawn(subs);
                                        }
                                    }
                                    types::Action::TakeBuy => {
                                        todo!()
                                    }
                                    types::Action::PayInvoice => {
                                        todo!()
                                    }
                                    types::Action::FiatSent => {
                                        let order_id = msg.order_id.unwrap();
                                        let order = match Order::by_id(&pool, order_id).await? {
                                            Some(order) => order,
                                            None => {
                                                error!("FiatSent: Order Id {order_id} not found!");
                                                break;
                                            }
                                        };
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
                                        let message = messages::buyer_sentfiat(event.pubkey)?;
                                        send_dm(&client, &my_keys, &seller_pubkey, message).await?;
                                        // We send a message to buyer to wait
                                        let message = messages::you_sent_fiat(seller_pubkey)?;
                                        send_dm(&client, &my_keys, &event.pubkey, message).await?;
                                    }
                                    types::Action::Release => {
                                        // TODO: Add validations
                                        // is the seller pubkey?
                                        let seller_pubkey = event.pubkey;
                                        let status = Status::SettledHoldInvoice;
                                        let order_id = msg.order_id.unwrap();
                                        let order = Order::by_id(&pool, order_id).await?.unwrap();
                                        if order.preimage.is_none() {
                                            break;
                                        }
                                        let preimage = order.preimage.as_ref().unwrap();
                                        ln_client.settle_hold_invoice(preimage).await?;
                                        info!("Order Id {}: Released sats", &order.id);
                                        // We publish a new replaceable kind nostr event with the status updated
                                        // and update on local database the status and new event id
                                        update_order_event(
                                            &pool, &client, &my_keys, status, &order,
                                        )
                                        .await?;
                                        // We send a message to seller
                                        let buyer_pubkey = XOnlyPublicKey::from_bech32(
                                            order.buyer_pubkey.as_ref().unwrap(),
                                        )?;
                                        let message = messages::sell_success(buyer_pubkey)?;
                                        send_dm(&client, &my_keys, &seller_pubkey, message).await?;

                                        // We send a *funds released* message to buyer
                                        let message = messages::funds_released(seller_pubkey)?;
                                        send_dm(&client, &my_keys, &buyer_pubkey, message).await?;
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
                                                                "Order Id {}: Invoice with hash: {} paid!",
                                                                order.id,
                                                                msg.payment.payment_hash
                                                            );
                                                            // Purchase completed message to buyer
                                                            let message =
                                                                messages::purchase_completed(
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
    use crate::models::NewOrder;
    use crate::types::Message;

    #[test]
    fn test_order_deserialize_serialize() {
        let sample_order = r#"{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","prime":1}"#;
        let order = NewOrder::from_json(sample_order).unwrap();
        let json_order = order.as_json().unwrap();
        assert_eq!(sample_order, json_order);
    }

    #[test]
    fn test_message_deserialize_serialize() {
        let sample_message = r#"{"version":0,"order_id":"7dd204d2-d06c-4406-a3d9-4415f4a8b9c9","action":"TakeSell","content":{"PaymentRequest":"lnbc1..."}}"#;
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
