use crate::util::publish_order;
use nostr::hashes::hex::ToHex;
use nostr::key::FromBech32;
use nostr::key::ToBech32;
use nostr::util::nips::nip04::decrypt;
use nostr::util::time::timestamp;
use nostr::{Kind, KindBase, SubscriptionFilter};
use nostr_sdk::{Client, RelayPoolNotifications};

pub mod db;
pub mod lightning;
pub mod models;
pub mod types;
pub mod util;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();
    // Connect to database
    let pool = crate::db::connect().await?;
    let my_keys = crate::util::get_keys()?;

    // Create new client
    let client = Client::new(&my_keys);

    // Add relays
    // client.add_relay("wss://relay.grunch.dev", None).await?;
    client
        .add_relay("wss://relay.sovereign-stack.org", None)
        .await?;
    // client.add_relay("wss://relay.damus.io", None).await?;
    // client.add_relay("wss://nostr.openchain.fr", None).await?;

    // Connect to relays and keep connection alive
    client.connect().await?;

    let subscription = SubscriptionFilter::new()
        .pubkey(my_keys.public_key())
        .since(timestamp());

    client.subscribe(vec![subscription]).await?;

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
                                types::Action::PaymentRequest => {
                                    // If a buyer sent me a lightning invoice we look on db an order with
                                    // that event id and save the buyer pubkey and invoice fields
                                    if let Some(payment_request) = msg.get_payment_request() {
                                        // TODO: Verify if payment_request is a valid lightning invoice
                                        let status = crate::types::Status::WaitingPayment;
                                        let buyer_pubkey = event.pubkey.to_bech32()?;
                                        let id = event.tags.iter().find(|t| {
                                            matches!(t.kind(), Ok(nostr::event::tag::TagKind::E))
                                        });
                                        if id.is_none() {
                                            continue;
                                        }
                                        let event_id = id.unwrap().content().unwrap();
                                        let db_order =
                                            crate::db::find_order_by_event_id(&pool, event_id)
                                                .await?;

                                        // Now we generate the hold invoice the seller need pay
                                        let (invoice_response, preimage, hash) =
                                            crate::lightning::create_hold_invoice(
                                                &db_order.description,
                                                db_order.amount,
                                            )
                                            .await?;
                                        crate::db::edit_order(
                                            &pool,
                                            &status,
                                            event_id,
                                            &buyer_pubkey,
                                            &payment_request,
                                            &preimage.to_hex(),
                                            &hash.to_hex(),
                                        )
                                        .await?;
                                        let seller_pubkey = db_order.seller_pubkey.unwrap();
                                        let seller_keys = nostr::key::Keys::from_bech32_public_key(
                                            &seller_pubkey,
                                        )?;
                                        // We send the hold invoice to the seller
                                        crate::util::send_dm(
                                            &client,
                                            &my_keys,
                                            &seller_keys,
                                            invoice_response.payment_request,
                                        )
                                        .await?;
                                        crate::lightning::subscribe_invoice(
                                            &client,
                                            &pool,
                                            &hash.to_hex(),
                                        )
                                        .await?;
                                    }
                                }
                                types::Action::FiatSent => println!("FiatSent"),
                                types::Action::Release => println!("Release"),
                            }
                        }
                    }
                };
            }
        }
    }

    pool.close().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::types::{Message, Order};

    #[test]
    fn test_order_deserialize_serialize() {
        let sample_order = r#"{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","prime":1,"created_at":1640839235}"#;
        let order = Order::from_json(&sample_order).unwrap();
        let json_order = order.as_json().unwrap();
        assert_eq!(sample_order, json_order);
    }

    #[test]
    fn test_message_deserialize_serialize() {
        let sample_message =
            r#"{"version":0,"action":"PaymentRequest","content":{"PaymentRequest":"lnbc1..."}}"#;
        let message = Message::from_json(&sample_message).unwrap();
        assert!(message.verify());
        let json_message = message.as_json().unwrap();
        assert_eq!(sample_message, json_message);
    }

    #[test]
    fn test_wrong_message_should_fail() {
        let sample_message = r#"{"version":0,"action":"PaymentRequest","content":{"Order":{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","prime":1,"payment_request":null,"created_at":1640839235}}}"#;
        let message = Message::from_json(&sample_message).unwrap();
        assert!(!message.verify());
    }
}
