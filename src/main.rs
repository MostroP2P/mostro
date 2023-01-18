use crate::util::publish_order;
use dotenvy::dotenv;
use log::info;
use nostr::hashes::hex::ToHex;
use nostr::util::nips::nip04::decrypt;
use nostr::util::nips::nip19::{FromBech32, ToBech32};
use nostr::util::time::timestamp;
use nostr::{Kind, KindBase, SubscriptionFilter};
use nostr_sdk::RelayPoolNotifications;
use sqlx_crud::Crud;
use tonic_openssl_lnd::lnrpc::invoice::InvoiceState;

pub mod db;
pub mod flow;
pub mod lightning;
pub mod messages;
pub mod models;
pub mod types;
pub mod util;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    pretty_env_logger::init();
    // Connect to database
    let pool = crate::db::connect().await?;
    // Connect to relays
    let client = crate::util::connect_nostr().await?;
    let my_keys = crate::util::get_keys()?;

    let subscription = SubscriptionFilter::new()
        .pubkey(my_keys.public_key())
        .since(timestamp());

    client.subscribe(vec![subscription]).await?;
    let mut ln_client = crate::lightning::LndConnector::new().await;

    loop {
        let mut notifications = client.notifications();

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotifications::ReceivedEvent(event) = notification {
                if let Kind::Base(KindBase::EncryptedDirectMessage) = event.kind {
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
                                    // TODO: Change this status to TakeSell
                                    types::Action::TakeSell => {
                                        // If a buyer sent me a lightning invoice we look on db an order with
                                        // that order id and save the buyer pubkey and invoice fields
                                        if let Some(payment_request) = msg.get_payment_request() {
                                            // TODO: Verify if payment_request is a valid lightning invoice
                                            let status = crate::types::Status::WaitingPayment;
                                            let buyer_pubkey = event.pubkey.to_bech32()?;
                                            let order_id = msg.order_id.unwrap();
                                            let db_order =
                                                crate::models::Order::by_id(&pool, order_id)
                                                    .await?
                                                    .unwrap();

                                            // Now we generate the hold invoice the seller need pay
                                            let (invoice_response, preimage, hash) = ln_client
                                                .create_hold_invoice(
                                                    &db_order.description,
                                                    db_order.amount,
                                                )
                                                .await?;
                                            crate::db::edit_order(
                                                &pool,
                                                &status,
                                                order_id,
                                                &buyer_pubkey,
                                                &payment_request,
                                                &preimage.to_hex(),
                                                &hash.to_hex(),
                                            )
                                            .await?;
                                            // We need to publish a new event with the new status
                                            crate::util::update_order_event(
                                                &pool, &client, &my_keys, status, &db_order,
                                            )
                                            .await?;
                                            let seller_keys = nostr::key::Keys::from_public_key(
                                                nostr::key::XOnlyPublicKey::from_bech32(
                                                    db_order.seller_pubkey.as_ref().unwrap(),
                                                )?,
                                            );
                                            let message = crate::messages::payment_request(
                                                &db_order,
                                                &invoice_response.payment_request,
                                            );
                                            // We send the hold invoice to the seller
                                            crate::util::send_dm(
                                                &client,
                                                &my_keys,
                                                &seller_keys,
                                                message,
                                            )
                                            .await?;
                                            let message =
                                                crate::messages::waiting_seller_to_pay_invoice(
                                                    db_order.id,
                                                );
                                            let buyer_keys =
                                                nostr::key::Keys::from_public_key(event.pubkey);

                                            // We send a message to buyer to know that seller was requested to pay the invoice
                                            crate::util::send_dm(
                                                &client,
                                                &my_keys,
                                                &buyer_keys,
                                                message,
                                            )
                                            .await?;
                                            let mut ln_client_invoices =
                                                crate::lightning::LndConnector::new().await;
                                            let (tx, mut rx) = tokio::sync::mpsc::channel(100);

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
                                                            crate::flow::hold_invoice_paid(&hash)
                                                                .await;
                                                            println!("Invoice with hash {hash} accepted!");
                                                        } else if msg.state == InvoiceState::Settled
                                                        {
                                                            // If the payment was released by the seller
                                                            println!(
                                                                "Invoice with hash {hash} settled!"
                                                            );
                                                            crate::flow::hold_invoice_settlement(
                                                                &hash,
                                                            )
                                                            .await;
                                                        } else if msg.state
                                                            == InvoiceState::Canceled
                                                        {
                                                            // If the payment was canceled
                                                            println!("Invoice with hash {hash} canceled!");
                                                            crate::flow::hold_invoice_canceled(
                                                                &hash,
                                                            )
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
                                        // TODO: Add validations
                                        // is the buyer pubkey?
                                        let buyer_pubkey = event.pubkey.to_bech32()?;
                                        let status = crate::types::Status::FiatSent;
                                        let order_id = msg.order_id.unwrap();
                                        let db_order = crate::models::Order::by_id(&pool, order_id)
                                            .await?
                                            .unwrap();

                                        // We publish a new replaceable kind nostr event with the status updated
                                        // and update on local database the status and new event id
                                        crate::util::update_order_event(
                                            &pool, &client, &my_keys, status, &db_order,
                                        )
                                        .await?;
                                        let seller_pubkey =
                                            db_order.seller_pubkey.as_ref().unwrap();
                                        let seller_keys = nostr::key::Keys::from_public_key(
                                            nostr::key::XOnlyPublicKey::from_bech32(seller_pubkey)?,
                                        );
                                        // We send a message to seller to release
                                        let message =
                                            crate::messages::buyer_sentfiat(&buyer_pubkey);
                                        crate::util::send_dm(
                                            &client,
                                            &my_keys,
                                            &seller_keys,
                                            message,
                                        )
                                        .await?;
                                        // We send a message to buyer to wait
                                        let message = crate::messages::you_sent_fiat(seller_pubkey);
                                        let buyer_keys = nostr::key::Keys::from_public_key(
                                            nostr::key::XOnlyPublicKey::from_bech32(seller_pubkey)?,
                                        );
                                        crate::util::send_dm(
                                            &client,
                                            &my_keys,
                                            &buyer_keys,
                                            message,
                                        )
                                        .await?;
                                    }
                                    types::Action::Release => {
                                        // TODO: Add validations
                                        // is the seller pubkey?
                                        let seller_pubkey = event.pubkey.to_bech32()?;
                                        let status = crate::types::Status::SettledHoldInvoice;
                                        let order_id = msg.order_id.unwrap();
                                        let db_order = crate::models::Order::by_id(&pool, order_id)
                                            .await?
                                            .unwrap();
                                        if db_order.preimage.is_none() {
                                            break;
                                        }
                                        let preimage = db_order.preimage.as_ref().unwrap();
                                        ln_client.settle_hold_invoice(preimage).await?;
                                        info!("Order Id: {} - Released sats", &db_order.id);
                                        // We publish a new replaceable kind nostr event with the status updated
                                        // and update on local database the status and new event id
                                        crate::util::update_order_event(
                                            &pool, &client, &my_keys, status, &db_order,
                                        )
                                        .await?;
                                        let seller_keys = nostr::key::Keys::from_public_key(
                                            nostr::key::XOnlyPublicKey::from_bech32(
                                                &seller_pubkey,
                                            )?,
                                        );
                                        // We send a message to seller
                                        let buyer_pubkey = db_order.buyer_pubkey.as_ref().unwrap();
                                        let message = crate::messages::sell_success(buyer_pubkey);
                                        crate::util::send_dm(
                                            &client,
                                            &my_keys,
                                            &seller_keys,
                                            message,
                                        )
                                        .await?;

                                        // We send a *funds released* message to buyer
                                        let message =
                                            crate::messages::funds_released(&seller_pubkey);
                                        let buyer_keys = nostr::key::Keys::from_public_key(
                                            nostr::key::XOnlyPublicKey::from_bech32(buyer_pubkey)?,
                                        );

                                        crate::util::send_dm(
                                            &client,
                                            &my_keys,
                                            &buyer_keys,
                                            message,
                                        )
                                        .await?;
                                        // Finally we try to pay buyer's invoice
                                        let payment_request =
                                            db_order.buyer_invoice.as_ref().unwrap();
                                        ln_client
                                            .send_payment(payment_request, db_order.amount)
                                            .await;
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
    use crate::types::{Message, Order};

    #[test]
    fn test_order_deserialize_serialize() {
        let sample_order = r#"{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","prime":1,"created_at":1640839235}"#;
        let order = Order::from_json(sample_order).unwrap();
        let json_order = order.as_json().unwrap();
        assert_eq!(sample_order, json_order);
    }

    #[test]
    fn test_message_deserialize_serialize() {
        let sample_message = r#"{"version":0,"order_id":54,"action":"TakeSell","content":{"PaymentRequest":"lnbc1..."}}"#;
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
