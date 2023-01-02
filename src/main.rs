use crate::util::{publish_order,print_orders_table};
use log::info;
use nostr::hashes::hex::ToHex;
use nostr::key::FromBech32;
use nostr::key::ToBech32;
use nostr::util::nips::nip04::decrypt;
use nostr::util::time::timestamp;
use nostr::{Kind, KindBase, SubscriptionFilter};
use nostr_sdk::RelayPoolNotifications;
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
                                    types::Action::ListOffers => {
                                        if let Some(st) = msg.get_order_list_status(){
                                            //Get pubkey of the client for dm
                                            let client_pubkey = event.pubkey.to_bech32()?;
                                            //Get orders from mostro database - selecting state - defaul is Pending
                                            let orders_db = crate::db::find_orders_by_event_status(&pool,&st.to_string()).await?;                                

                                            //Print table of orders
                                            let message = print_orders_table(orders_db)?;  
                                                                                    
                                            let client_pubkey =
                                                nostr::key::Keys::from_bech32_public_key(client_pubkey)?;

                                            // We send a message with a list of pending orders
                                            crate::util::send_dm(
                                                &client,
                                                &my_keys,
                                                &client_pubkey,
                                                message,
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
                                            let event_id =
                                                crate::util::get_event_id_from_dm(&event)?;
                                            let db_order =
                                                crate::db::find_order_by_event_id(&pool, &event_id)
                                                    .await?;

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
                                                &event_id,
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

                                            let seller_pubkey =
                                                db_order.seller_pubkey.as_ref().unwrap();
                                            let seller_keys =
                                                nostr::key::Keys::from_bech32_public_key(
                                                    seller_pubkey,
                                                )?;
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
                                                nostr::key::Keys::from_bech32_public_key(
                                                    buyer_pubkey,
                                                )?;
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
                                            // Receiving msgs from the invoice subscrition.
                                            while let Some(msg) = rx.recv().await {
                                                let hash = msg.hash.to_hex();
                                                // If this invoice was paid by the seller
                                                if msg.state == InvoiceState::Accepted {
                                                    crate::flow::hold_invoice_paid(
                                                        &hash, &pool, &client,
                                                    )
                                                    .await;
                                                } else if msg.state == InvoiceState::Settled {
                                                    // If this invoice was Settled we can do something with it
                                                    crate::flow::hold_invoice_settlement(
                                                        &hash, &pool, &client,
                                                    )
                                                    .await;
                                                } else if msg.state == InvoiceState::Canceled {
                                                    // If this invoice was Canceled
                                                    crate::flow::hold_invoice_canceled(
                                                        &hash, &pool, &client,
                                                    )
                                                    .await;
                                                } else {
                                                    info!("Invoice with hash: {hash} subscribed!");
                                                }
                                            }
                                        }
                                    }
                                    types::Action::FiatSent => {
                                        // TODO: Add validations
                                        // is the buyer pubkey?
                                        let buyer_pubkey = event.pubkey.to_bech32()?;
                                        let status = crate::types::Status::FiatSent;
                                        let event_id = crate::util::get_event_id_from_dm(&event)?;

                                        let db_order =
                                            crate::db::find_order_by_event_id(&pool, &event_id)
                                                .await?;
                                        // We publish a new kind 11000 nostr event with the status updated
                                        // and update on local database the status and new event id
                                        crate::util::update_order_event(
                                            &pool, &client, &my_keys, status, &db_order,
                                        )
                                        .await?;
                                        let seller_pubkey =
                                            db_order.seller_pubkey.as_ref().unwrap();
                                        let seller_keys = nostr::key::Keys::from_bech32_public_key(
                                            seller_pubkey,
                                        )?;
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
                                        let buyer_keys =
                                            nostr::key::Keys::from_bech32_public_key(buyer_pubkey)?;
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
                                        let status = crate::types::Status::SettledInvoice;
                                        let event_id = crate::util::get_event_id_from_dm(&event)?;
                                        let db_order =
                                            crate::db::find_order_by_event_id(&pool, &event_id)
                                                .await?;
                                        if db_order.preimage.is_none() {
                                            break;
                                        }
                                        let preimage = db_order.preimage.as_ref().unwrap();
                                        ln_client.settle_hold_invoice(preimage).await?;
                                        info!("Order Id: {} - Released sats", &db_order.id);
                                        // We publish a new kind 11000 nostr event with the status updated
                                        // and update on local database the status and new event id
                                        crate::util::update_order_event(
                                            &pool, &client, &my_keys, status, &db_order,
                                        )
                                        .await?;
                                        let seller_keys = nostr::key::Keys::from_bech32_public_key(
                                            &seller_pubkey,
                                        )?;
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
                                        let buyer_keys =
                                            nostr::key::Keys::from_bech32_public_key(buyer_pubkey)?;
                                        crate::util::send_dm(
                                            &client,
                                            &my_keys,
                                            &buyer_keys,
                                            message,
                                        )
                                        .await?;
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
