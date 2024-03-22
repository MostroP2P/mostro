use crate::cli::settings::Settings;
use crate::db;
use crate::error::MostroError;
use crate::flow;
use crate::lightning;
use crate::lightning::LndConnector;
use crate::messages;
use crate::models::Yadio;
use crate::nip33::{new_event, order_to_tags};
use crate::NOSTR_CLIENT;

use anyhow::{Context, Result};
use mostro_core::message::{Action, Content, Message};
use mostro_core::order::{Kind as OrderKind, Order, SmallOrder, Status};
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
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

pub type FiatNames = std::collections::HashMap<String, String>;
const MAX_RETRY: u16 = 4;

pub async fn retries_yadio_request(
    req_string: &str,
    fiat_code: &str,
) -> Result<(Option<reqwest::Response>, bool)> {
    // Get Fiat list and check if currency exchange is available
    let api_req_string = "https://api.yadio.io/currencies".to_string();
    let fiat_list_check = reqwest::get(api_req_string)
        .await?
        .json::<FiatNames>()
        .await?
        .contains_key(fiat_code);

    // Exit with error - no currency
    if !fiat_list_check {
        return Ok((None, fiat_list_check));
    }

    let res = reqwest::get(req_string)
        .await
        .context("Something went wrong with API request, try again!")?;

    Ok((Some(res), fiat_list_check))
}

/// Request market quote from Yadio to have sats amount at actual market price
pub async fn get_market_quote(
    fiat_amount: &i64,
    fiat_code: &str,
    premium: i64,
) -> Result<i64, MostroError> {
    // Add here check for market price
    let req_string = format!(
        "https://api.yadio.io/convert/{}/{}/BTC",
        fiat_amount, fiat_code
    );
    info!("Requesting API price: {}", req_string);

    let mut req = (None, false);
    let mut no_answer_api = false;

    // Retry for 4 times
    for retries_num in 1..=MAX_RETRY {
        match retries_yadio_request(&req_string, fiat_code).await {
            Ok(response) => {
                req = response;
                break;
            }
            Err(_e) => {
                if retries_num == MAX_RETRY {
                    no_answer_api = true;
                }
                println!(
                    "API price request failed retrying - {} tentatives left.",
                    (MAX_RETRY - retries_num)
                );
                thread::sleep(std::time::Duration::from_secs(2));
            }
        };
    }

    // Case no answers from Yadio
    if no_answer_api {
        return Err(MostroError::NoAPIResponse);
    }

    // No currency present
    if !req.1 {
        return Err(MostroError::NoCurrency);
    }

    let quote = req.0.unwrap().json::<Yadio>().await;
    if quote.is_err() {
        return Err(MostroError::MalformedAPIRes);
    }
    let quote = quote.unwrap();

    let mut sats = quote.result * 100_000_000_f64;

    // Added premium value to have correct sats value
    if premium != 0 {
        sats += (premium as f64) / 100_f64 * sats;
    }

    Ok(sats as i64)
}

pub fn get_fee(amount: i64) -> i64 {
    let mostro_settings = Settings::get_mostro();
    // We calculate the bot fee
    let split_fee = (mostro_settings.fee * amount as f64) / 2.0;
    split_fee.round() as i64
}

pub async fn publish_order(
    pool: &SqlitePool,
    keys: &Keys,
    new_order: &SmallOrder,
    initiator_pubkey: &str,
    master_pubkey: &str,
    ack_pubkey: XOnlyPublicKey,
) -> Result<()> {
    let mut fee = 0;
    if new_order.amount > 0 {
        fee = get_fee(new_order.amount);
    }
    // Prepare a new default order
    let mut new_order_db = Order {
        id: Uuid::new_v4(),
        kind: OrderKind::Sell.to_string(),
        status: Status::Pending.to_string(),
        creator_pubkey: initiator_pubkey.to_string(),
        payment_method: new_order.payment_method.clone(),
        amount: new_order.amount,
        fee,
        fiat_code: new_order.fiat_code.clone(),
        fiat_amount: new_order.fiat_amount,
        premium: new_order.premium,
        buyer_invoice: new_order.buyer_invoice.clone(),
        created_at: Timestamp::now().as_i64(),
        ..Default::default()
    };

    if new_order.kind == Some(OrderKind::Buy) {
        new_order_db.kind = OrderKind::Buy.to_string();
        new_order_db.buyer_pubkey = Some(initiator_pubkey.to_string());
        new_order_db.master_buyer_pubkey = Some(master_pubkey.to_string());
    } else {
        new_order_db.seller_pubkey = Some(initiator_pubkey.to_string());
        new_order_db.master_seller_pubkey = Some(master_pubkey.to_string());
    }

    // Request price from API in case amount is 0
    new_order_db.price_from_api = new_order.amount == 0;

    // CRUD order creation
    let mut order = new_order_db.clone().create(pool).await?;
    let order_id = order.id;
    info!("New order saved Id: {}", order_id);
    // We transform the order fields to tags to use in the event
    let tags = order_to_tags(&new_order_db);

    info!("order tags to be published: {:#?}", tags);
    // nip33 kind with order fields as tags and order id as identifier
    let event = new_event(keys, "", order_id.to_string(), tags)?;
    info!("Order event to be published: {event:#?}");
    let event_id = event.id.to_string();
    info!("Publishing Event Id: {event_id} for Order Id: {order_id}");
    // We update the order with the new event_id
    order.event_id = event_id;
    order.update(pool).await?;
    let mut order = new_order_db.as_new_order();
    order.id = Some(order_id);

    // Send message as ack with small order
    let ack_message = Message::new_order(
        order.id,
        None,
        Action::NewOrder,
        Some(Content::Order(order)),
    );
    let ack_message = ack_message.as_json()?;

    send_dm(keys, &ack_pubkey, ack_message).await?;

    NOSTR_CLIENT.get().unwrap()
        .send_event(event)
        .await
        .map(|_s| ())
        .map_err(|err| err.into())
}

pub async fn send_dm(
    sender_keys: &Keys,
    receiver_pubkey: &XOnlyPublicKey,
    content: String,
) -> Result<()> {
    info!("DM content: {content:#?}");
    let event =
        EventBuilder::new_encrypted_direct_msg(sender_keys, *receiver_pubkey, content, None)?
            .to_event(sender_keys)?;
    info!("Sending event: {event:#?}");
    NOSTR_CLIENT.get().unwrap().send_event(event).await?;

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
    client: &Client,
    keys: &Keys,
    status: Status,
    order: &Order,
) -> Result<Order> {
    let mut order_updated = order.clone();
    // update order.status with new status
    order_updated.status = status.to_string();
    // We transform the order fields to tags to use in the event
    let tags = order_to_tags(&order_updated);
    // nip33 kind with order id as identifier and order fields as tags
    let event = new_event(keys, "", order.id.to_string(), tags)?;
    let order_id = order.id.to_string();
    info!("Sending replaceable event: {event:#?}");
    // We update the order with the new event_id
    order_updated.event_id = event.id.to_string();

    info!(
        "Order Id: {} updated Nostr new Status: {}",
        order_id,
        status.to_string()
    );

    client.send_event(event).await?;

    println!(
        "Inside update_order_event order_updated status {:?} - order id {:?}",
        order_updated.status, order_updated.id,
    );

    Ok(order_updated)
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
    client: &Client,
    my_keys: &Keys,
    payment_request: Option<String>,
    buyer_pubkey: &XOnlyPublicKey,
    seller_pubkey: &XOnlyPublicKey,
    mut order: Order,
) -> anyhow::Result<()> {
    let mut ln_client = lightning::LndConnector::new().await;
    // Add fee of seller to hold invoice
    let new_amount = order.amount + order.fee;

    // Now we generate the hold invoice that seller should pay
    let (invoice_response, preimage, hash) = ln_client
        .create_hold_invoice(
            &messages::hold_invoice_description(
                my_keys.public_key(),
                &order.id.to_string(),
                &order.fiat_code,
                &order.fiat_amount.to_string(),
            )?,
            new_amount,
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

    // We need to publish a new event with the new status
    let pool = db::connect().await?;
    let order_updated = update_order_event(client, my_keys, Status::WaitingPayment, &order).await?;
    order_updated.update(&pool).await?;

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

pub async fn get_market_amount_and_fee(
    fiat_amount: i64,
    fiat_code: &str,
    premium: i64,
) -> Result<(i64, i64)> {
    // Update amount order
    let new_sats_amount = get_market_quote(&fiat_amount, fiat_code, premium).await?;
    let fee = get_fee(new_sats_amount);

    Ok((new_sats_amount, fee))
}

/// Set order sats amount, this used when a buyer take a sell order
pub async fn set_waiting_invoice_status(
    order: &mut Order,
    buyer_pubkey: XOnlyPublicKey,
    my_keys: &Keys,
    client: &Client,
) -> Result<i64> {
    let kind = OrderKind::from_str(&order.kind).unwrap();
    let status = Status::WaitingBuyerInvoice;

    let buyer_final_amount = order.amount - order.fee;
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
    send_dm(client, my_keys, &buyer_pubkey, message.as_json()?).await?;

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
    ln_client: &mut LndConnector,
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
        send_cant_do_msg(Some(order.id), None, &event.pubkey, client).await;
        return Ok(());
    }
    if order.preimage.is_none() {
        return Ok(());
    }

    // Settling the hold invoice
    let preimage = order.preimage.as_ref().unwrap();
    ln_client.settle_hold_invoice(preimage).await?;
    info!("{action}: Order Id {}: hold invoice settled", order.id);

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

pub async fn send_cant_do_msg(order_id: Option<Uuid>, message: Option<String>, destination_key : &XOnlyPublicKey, client: &Client){
    // Get mostro keys
    let my_keys = crate::util::get_keys().unwrap();
    // Prepare content in case
    let content = if let Some( m ) = message {
        Some(Content::TextMessage(m))
    }
    else{
        None
    };
    
    // Send message to event creator
    let message = Message::cant_do(
        Some(order_id),
        None,
        content,
    );
    if let Ok(message) = message.as_json(){
        let _ = send_dm(client, &my_keys, destination_key, message).await;
    }   

}
