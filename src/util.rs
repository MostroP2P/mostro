use crate::cli::settings::Settings;
use crate::error::MostroError;
use crate::flow;
use crate::lightning;
use crate::lightning::LndConnector;
use crate::messages;
use crate::models::Yadio;
use crate::nip33::{new_event, order_to_tags};

use anyhow::{Context, Result};
use mostro_core::message::{Action, Content, Message};
use mostro_core::order::{Kind as OrderKind, Order, SmallOrder, Status};
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::fmt::Write;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc::channel;
use tokio::sync::Mutex;
use tonic_openssl_lnd::lnrpc::invoice::InvoiceState;
use tracing::error;
use tracing::info;
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
    info!("Requesting API price: {}", req_string);

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
        return Err(MostroError::NoAPIResponse);
    }

    let quote = req.unwrap().json::<Yadio>().await;
    if quote.is_err() {
        return Err(MostroError::NoAPIResponse);
    }
    let quote = quote.unwrap();

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
    new_order: &SmallOrder,
    initiator_pubkey: &str,
    master_pubkey: &str,
    ack_pubkey: XOnlyPublicKey,
) -> Result<()> {
    let order = crate::db::add_order(pool, new_order, "", initiator_pubkey, master_pubkey).await?;
    let order_id = order.id;
    info!("New order saved Id: {}", order_id);
    // We transform the order fields to tags to use in the event
    let tags = order_to_tags(&order);

    info!("order tags to be published: {:#?}", tags);
    // nip33 kind with order fields as tags and order id as identifier
    let event = new_event(keys, "", order_id.to_string(), tags)?;
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
    let mut order = order.as_new_order();
    order.id = Some(order_id);

    // Send message as ack with small order
    let ack_message = Message::new_order(
        order.id,
        None,
        Action::NewOrder,
        Some(Content::Order(order)),
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
    user: &str,
    buyer_sent_rate: bool,
    seller_sent_rate: bool,
    tags: Vec<(String, String)>,
    order_id: Uuid,
    keys: &Keys,
    pool: &SqlitePool,
    rate_list: Arc<Mutex<Vec<Event>>>,
) -> Result<()> {
    // Get order from id
    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    }; // nip33 kind with user as identifier
    let event = new_event(keys, "", user.to_string(), tags)?;
    info!("Sending replaceable event: {event:#?}");
    // We update the order vote status
    if buyer_sent_rate {
        order.buyer_sent_rate = buyer_sent_rate;
    }
    if seller_sent_rate {
        order.seller_sent_rate = seller_sent_rate;
    }
    order.update(pool).await?;

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
    let amount = amount.unwrap_or(order.amount);
    let mut order = order.clone();
    // update order.status with new status
    order.status = status.to_string();
    // We transform the order fields to tags to use in the event
    let tags = order_to_tags(&order);
    // nip33 kind with order id as identifier and order fields as tags
    let event = new_event(keys, "", order.id.to_string(), tags)?;
    let event_id = event.id.to_string();
    let status_str = status.to_string();
    info!("Sending replaceable event: {event:#?}");
    // We update the order id with the new event_id
    crate::db::update_order_event_id_status(pool, order.id, &status, &event_id, amount).await?;
    info!(
        "Order Id: {} updated Nostr new Status: {}",
        order.id, status_str
    );

    client.send_event(event).await?;

    Ok(())
}

pub async fn connect_nostr() -> Result<Client> {
    let my_keys = crate::util::get_keys()?;
    let nostr_settings = Settings::get_nostr();
    // Create new client
    let client = Client::new(&my_keys);
    let relays = nostr_settings.relays;

    // Add relays
    for r in relays.into_iter() {
        let opts = RelayOptions::new();
        client.add_relay_with_opts(r, None, opts).await?;
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
    order_id: Uuid,
) -> anyhow::Result<()> {
    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };
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
        order.buyer_invoice = Some(invoice);
    };

    // Using CRUD to update all fiels
    order.preimage = Some(bytes_to_string(&preimage));
    order.hash = Some(bytes_to_string(&hash));
    order.status = Status::WaitingPayment.to_string();
    order.buyer_pubkey = Some(buyer_pubkey.to_string());
    order.seller_pubkey = Some(seller_pubkey.to_string());
    let order = order.update(pool).await?;

    // We need to publish a new event with the new status
    update_order_event(pool, client, my_keys, Status::WaitingPayment, &order, None).await?;
    let mut new_order = order.as_new_order();
    new_order.status = Some(Status::WaitingPayment);
    // We create a Message to send the hold invoice to seller
    let message = Message::new_order(
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

    let message = Message::new_order(Some(order.id), None, Action::WaitingSellerToPay, None);
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
                let hash = bytes_to_string(msg.hash.as_ref());
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
    let kind = OrderKind::from_str(&order.kind).unwrap();
    let status = Status::from_str(&order.status).unwrap();

    // We send this data related to the buyer
    let order_data = SmallOrder::new(
        Some(order.id),
        Some(kind),
        Some(status),
        buyer_final_amount,
        order.fiat_code.clone(),
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        None,
        None,
        None,
        None,
    );
    // We create a Message
    let message = Message::new_order(
        Some(order.id),
        None,
        Action::AddInvoice,
        Some(Content::Order(order_data)),
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
    order: &Order,
) -> Result<()> {
    // Send dm to counterparts
    let message_to_parties = Message::new_order(Some(order.id), None, Action::RateUser, None);
    let message_to_parties = message_to_parties.as_json().unwrap();
    // to buyer
    send_dm(client, my_keys, buyer_pubkey, message_to_parties.clone()).await?;
    // to seller
    send_dm(client, my_keys, seller_pubkey, message_to_parties).await?;

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
        my_keys.public_key().to_string()
    } else {
        order.seller_pubkey.as_ref().unwrap().to_string()
    };
    // Check if the pubkey is right
    if event.pubkey.to_string() != pubkey {
        // We create a Message
        let message = Message::cant_do(Some(order.id), None, None);
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

pub fn bytes_to_string(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut output, b| {
        let _ = write!(output, "{:02x}", b);
        output
    })
}

pub fn nostr_tags_to_tuple(tags: Vec<Tag>) -> Vec<(String, String)> {
    let mut tags_tuple = Vec::new();
    for tag in tags {
        let t = tag.as_vec();
        tags_tuple.push((t[0].to_string(), t[1].to_string()));
    }

    tags_tuple
}
