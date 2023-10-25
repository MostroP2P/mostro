use crate::cli::settings::Settings;
use crate::error::MostroError;
use crate::lightning;
use crate::lightning::LndConnector;
use crate::messages;
use crate::models::Yadio;
use crate::nip33::{new_event, order_to_tags};
use crate::{db, flow};

use anyhow::{Context, Result};
use log::{error, info};
use mostro_core::order::{Kind as OrderKind, NewOrder, Order, SmallOrder, Status};
use mostro_core::{Action, Content, Message};
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
use sqlx::{Pool, Sqlite};
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc::channel;
use tokio::sync::Mutex;
use tonic_openssl_lnd::lnrpc::invoice::InvoiceState;
use uuid::Uuid;

pub async fn retries_yadio_request(req_string: &String) -> Result<reqwest::Response> {
    let res = reqwest::get(req_string)
        .await
        .context("Something went wrong with API request, try again!")?;

    Ok(res)
}

/// Request market quote from Yadio to have sats amount at actual market price
pub async fn get_market_quote(
    fiat_amount: &i64,
    fiat_code: &str,
    premium: &i64,
) -> Result<i64, MostroError> {
    // Add here check for market price
    let req_string = format!(
        "https://api.yadio.io/convert/{}/{}/BTC",
        fiat_amount, fiat_code
    );

    let mut req = None;
    // Retry for 4 times
    for retries_num in 1..=4 {
        match retries_yadio_request(&req_string).await {
            Ok(response) => {
                req = Some(response);
                break;
            }
            Err(_e) => {
                println!(
                    "API price request failed retrying - {} tentatives left.",
                    (4 - retries_num)
                );
                thread::sleep(std::time::Duration::from_secs(2));
            }
        };
    }

    // Case no answers from Yadio
    if req.is_none() {
        println!("Send dm to user to signal no API response");
        return Err(MostroError::NoAPIResponse);
    }

    let quote = req.unwrap().json::<Yadio>().await?;

    let mut sats = quote.result * 100_000_000_f64;

    // Added premium value to have correct sats value
    if *premium != 0 {
        sats += (*premium as f64) / 100_f64 * sats;
    }

    Ok(sats as i64)
}

pub async fn publish_order(
    pool: &SqlitePool,
    client: &Client,
    keys: &Keys,
    new_order: &NewOrder,
    initiator_pubkey: &str,
    master_pubkey: &str,
    ack_pubkey: XOnlyPublicKey,
) -> Result<()> {
    let order = crate::db::add_order(pool, new_order, "", initiator_pubkey, master_pubkey).await?;
    let order_id = order.id;
    info!("New order saved Id: {}", order_id);
    // We transform the order fields to tags to use in the event
    let tags = order_to_tags(&order);
    // Now we have the order id, we can create a new event adding this id to the Order object
    let order = NewOrder::new(
        Some(order_id),
        OrderKind::from_str(&order.kind).unwrap(),
        Status::Pending,
        order.amount,
        order.fiat_code,
        order.fiat_amount,
        order.payment_method,
        order.premium,
        None,
        None,
        None,
        order.created_at,
    );
    let order_string = order.as_json().unwrap();
    info!("serialized order: {order_string}");
    // nip33 kind with order fields as tags and order id as identifier
    let event = new_event(keys, order_string, order_id.to_string(), tags)?;
    info!("Order event to be published: {event:#?}");
    let event_id = event.id.to_string();
    info!("Publishing Event Id: {event_id} for Order Id: {order_id}");
    // We update the order id with the new event_id
    crate::db::update_order_event_id_status(
        pool,
        order_id,
        &Status::Pending,
        &event_id,
        order.amount,
    )
    .await?;

    // Send message as ack with small order
    let ack_message = Message::new(
        0,
        order.id,
        None,
        Action::Order,
        Some(Content::Order(order.clone())),
    );
    let ack_message = ack_message.as_json()?;

    send_dm(client, keys, &ack_pubkey, ack_message).await?;

    client
        .send_event(event)
        .await
        .map(|_s| ())
        .map_err(|err| err.into())
}

pub async fn send_dm(
    client: &Client,
    sender_keys: &Keys,
    receiver_pubkey: &XOnlyPublicKey,
    content: String,
) -> Result<()> {
    info!("DM content: {content:#?}");
    let event =
        EventBuilder::new_encrypted_direct_msg(sender_keys, *receiver_pubkey, content, None)?
            .to_event(sender_keys)?;
    info!("Sending event: {event:#?}");
    client.send_event(event).await?;

    Ok(())
}

pub fn get_keys() -> Result<Keys> {
    let nostr_settings = Settings::get_nostr();
    // nostr private key
    let my_keys = Keys::from_sk_str(&nostr_settings.nsec_privkey)?;

    Ok(my_keys)
}

#[allow(clippy::too_many_arguments)]
pub async fn update_user_rating_event(
    user: &String,
    buyer_sent_rate: bool,
    seller_sent_rate: bool,
    reputation: String,
    order_id: Uuid,
    keys: &Keys,
    pool: &SqlitePool,
    rate_list: Arc<Mutex<Vec<Event>>>,
) -> Result<()> {
    // nip33 kind with user as identifier
    let event = new_event(keys, reputation, user.to_string(), vec![])?;
    info!("Sending replaceable event: {event:#?}");
    // We update the order vote status
    if buyer_sent_rate {
        crate::db::update_order_event_buyer_rate(pool, order_id, buyer_sent_rate).await?;
    }
    if seller_sent_rate {
        crate::db::update_order_event_seller_rate(pool, order_id, seller_sent_rate).await?;
    }

    // Add event message to global list
    rate_list.lock().await.push(event);

    Ok(())
}

pub async fn update_order_event(
    pool: &SqlitePool,
    client: &Client,
    keys: &Keys,
    status: Status,
    order: &Order,
    amount: Option<i64>,
) -> Result<()> {
    let kind = OrderKind::from_str(&order.kind).unwrap();
    let amount = amount.unwrap_or(order.amount);
    let publish_order = NewOrder::new(
        Some(order.id),
        kind,
        status,
        amount,
        order.fiat_code.to_owned(),
        order.fiat_amount,
        order.payment_method.to_owned(),
        order.premium,
        None,
        None,
        None,
        order.created_at,
    );
    let order_content = publish_order.as_json()?;
    let mut order = order.clone();
    // update order.status with new status
    order.status = status.to_string();
    // We transform the order fields to tags to use in the event
    let tags = order_to_tags(&order);
    // nip33 kind with order id as identifier and order fields as tags
    let event = new_event(keys, order_content, order.id.to_string(), tags)?;
    let event_id = event.id.to_string();
    let status_str = status.to_string();
    info!("Sending replaceable event: {event:#?}");
    // We update the order id with the new event_id
    crate::db::update_order_event_id_status(pool, order.id, &status, &event_id, amount).await?;
    info!(
        "Order Id: {} updated Nostr new Status: {}",
        order.id, status_str
    );

    client.send_event(event).await.map(|_s| ()).map_err(|err| {
        error!("{}", err);
        err.into()
    })
}

pub async fn connect_nostr() -> Result<Client> {
    let my_keys = crate::util::get_keys()?;
    let nostr_settings = Settings::get_nostr();
    // Create new client
    let client = Client::new(&my_keys);
    let relays = nostr_settings.relays;

    // Add relays
    for r in relays.into_iter() {
        client.add_relay(r, None).await?;
    }

    // Connect to relays and keep connection alive
    client.connect().await;

    Ok(client)
}

pub async fn show_hold_invoice(
    pool: &SqlitePool,
    client: &Client,
    my_keys: &Keys,
    payment_request: Option<String>,
    buyer_pubkey: &XOnlyPublicKey,
    seller_pubkey: &XOnlyPublicKey,
    order: &Order,
) -> anyhow::Result<()> {
    let mut ln_client = lightning::LndConnector::new().await;
    let mostro_settings = Settings::get_mostro();
    // Add fee of seller to hold invoice
    let seller_fee = mostro_settings.fee / 2.0;
    let add_fee = seller_fee * order.amount as f64;
    let rounded_fee = add_fee.round();
    let new_amount = order.amount + rounded_fee as i64;
    let seller_total_amount = new_amount;

    // Now we generate the hold invoice that seller should pay
    let (invoice_response, preimage, hash) = ln_client
        .create_hold_invoice(
            &messages::hold_invoice_description(
                my_keys.public_key(),
                &order.id.to_string(),
                &order.fiat_code,
                &order.fiat_amount.to_string(),
            )?,
            seller_total_amount,
        )
        .await?;
    if let Some(invoice) = payment_request {
        db::edit_buyer_invoice_order(pool, order.id, &invoice).await?;
    };
    let preimage: String = preimage.iter().map(|b| format!("{:02x}", b)).collect();
    let hash_str: String = hash.iter().map(|b| format!("{:02x}", b)).collect();

    db::edit_order(
        pool,
        &Status::WaitingPayment,
        order.id,
        buyer_pubkey,
        seller_pubkey,
        &preimage,
        &hash_str,
    )
    .await?;
    // We need to publish a new event with the new status
    update_order_event(pool, client, my_keys, Status::WaitingPayment, order, None).await?;
    let new_order = order.as_new_order();
    // We create a Message to send the hold invoice to seller
    let message = Message::new(
        0,
        Some(order.id),
        None,
        Action::PayInvoice,
        Some(Content::PaymentRequest(
            Some(new_order),
            invoice_response.payment_request,
        )),
    );
    let message = message.as_json()?;

    // We send the hold invoice to the seller
    send_dm(client, my_keys, seller_pubkey, message).await?;

    let message = Message::new(0, Some(order.id), None, Action::WaitingSellerToPay, None);
    let message = message.as_json()?;

    // We send a message to buyer to know that seller was requested to pay the invoice
    send_dm(client, my_keys, buyer_pubkey, message).await?;
    let mut ln_client_invoices = lightning::LndConnector::new().await;
    let (tx, mut rx) = channel(100);

    let invoice_task = {
        async move {
            ln_client_invoices.subscribe_invoice(hash, tx).await;
        }
    };
    tokio::spawn(invoice_task);
    let subs = {
        async move {
            // Receiving msgs from the invoice subscription.
            while let Some(msg) = rx.recv().await {
                let hash: String = msg.hash.iter().map(|b| format!("{:02x}", b)).collect();
                // If this invoice was paid by the seller
                if msg.state == InvoiceState::Accepted {
                    flow::hold_invoice_paid(&hash).await;
                    info!("Invoice with hash {hash} accepted!");
                } else if msg.state == InvoiceState::Settled {
                    // If the payment was settled
                    flow::hold_invoice_settlement(&hash).await;
                } else if msg.state == InvoiceState::Canceled {
                    // If the payment was canceled
                    flow::hold_invoice_canceled(&hash).await;
                } else {
                    info!("Invoice with hash: {hash} subscribed!");
                }
            }
        }
    };
    tokio::spawn(subs);

    Ok(())
}

/// Set market order sats amount, this used when a buyer take a sell order
pub async fn set_market_order_sats_amount(
    order: &mut Order,
    buyer_pubkey: XOnlyPublicKey,
    my_keys: &Keys,
    pool: &SqlitePool,
    client: &Client,
) -> Result<i64> {
    let mostro_settings = Settings::get_mostro();
    // Update amount order
    let new_sats_amount =
        get_market_quote(&order.fiat_amount, &order.fiat_code, &order.premium).await?;

    // We calculate the bot fee
    let fee = mostro_settings.fee / 2.0;
    let sub_fee = fee * new_sats_amount as f64;
    let rounded_fee = sub_fee.round();

    let buyer_final_amount = new_sats_amount - rounded_fee as i64;

    // We send this data related to the buyer
    let order_data = SmallOrder::new(
        order.id,
        buyer_final_amount,
        order.fiat_code.clone(),
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        None,
        None,
    );
    // We create a Message
    let message = Message::new(
        0,
        Some(order.id),
        None,
        Action::AddInvoice,
        Some(Content::SmallOrder(order_data)),
    );
    let message = message.as_json()?;

    send_dm(client, my_keys, &buyer_pubkey, message).await?;

    // Update order with new sats value
    order.amount = new_sats_amount;
    update_order_event(
        pool,
        client,
        my_keys,
        Status::WaitingBuyerInvoice,
        order,
        None,
    )
    .await?;

    Ok(order.amount)
}

/// Send message to buyer and seller to vote for counterpart
pub async fn rate_counterpart(
    client: &Client,
    buyer_pubkey: &XOnlyPublicKey,
    seller_pubkey: &XOnlyPublicKey,
    my_keys: &Keys,
    order: NewOrder,
) -> Result<()> {
    // Send dm to counterparts
    // to buyer
    let message_to_buyer = Message::new(0, order.id, None, Action::RateUser, None);
    let message_to_buyer = message_to_buyer.as_json().unwrap();
    send_dm(client, my_keys, buyer_pubkey, message_to_buyer).await?;
    // to seller
    let message_to_seller = Message::new(0, order.id, None, Action::RateUser, None);
    let message_to_seller = message_to_seller.as_json().unwrap();
    send_dm(client, my_keys, seller_pubkey, message_to_seller).await?;

    Ok(())
}

/// Settle a seller hold invoice
#[allow(clippy::too_many_arguments)]
pub async fn settle_seller_hold_invoice(
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
    status: Status,
    action: Action,
    is_admin: bool,
    order: &Order,
) -> Result<()> {
    // It can be settle only by a seller or by admin
    let pubkey = if is_admin {
        my_keys.public_key().to_bech32()?
    } else {
        order.seller_pubkey.as_ref().unwrap().to_string()
    };
    // Check if the pubkey is right
    if event.pubkey.to_bech32()? != pubkey {
        // We create a Message
        let message = Message::new(0, Some(order.id), None, Action::CantDo, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }
    if order.preimage.is_none() {
        return Ok(());
    }

    // Settling the hold invoice
    let preimage = order.preimage.as_ref().unwrap();
    ln_client.settle_hold_invoice(preimage).await?;
    info!("{action}: Order Id {}: hold invoice settled", order.id);
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    update_order_event(pool, client, my_keys, status, order, None).await?;

    Ok(())
}
