use crate::app::rate_user::get_user_reputation;
use crate::bitcoin_price::BitcoinPriceManager;
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

use anyhow::{Context, Error, Result};
use chrono::Duration;
use mostro_core::message::CantDoReason;
use mostro_core::message::{Action, Message, Payload};
use mostro_core::order::{Kind as OrderKind, Order, SmallOrder, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
use sqlx_crud::Crud;
use std::fmt::Write;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc::channel;
use tokio::sync::Mutex;
// use fedimint_tonic_lnd::Client;
use fedimint_tonic_lnd::lnrpc::invoice::InvoiceState;
use std::collections::HashMap;
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

pub fn get_bitcoin_price(fiat_code: &str) -> Result<f64> {
    BitcoinPriceManager::get_price(fiat_code)
        .ok_or_else(|| anyhow::anyhow!("Failed to get Bitcoin price"))
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

    if req.0.is_none() {
        return Err(MostroError::MalformedAPIRes);
    }

    let quote = if let Some(q) = req.0 {
        q.json::<Yadio>().await?
    } else {
        return Err(MostroError::MalformedAPIRes);
    };

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

pub fn get_expiration_date(expire: Option<i64>) -> i64 {
    let mostro_settings = Settings::get_mostro();
    // We calculate order expiration
    let expire_date: i64;
    let expires_at_max: i64 = Timestamp::now().as_u64() as i64
        + Duration::days(mostro_settings.max_expiration_days.into()).num_seconds();
    if let Some(mut exp) = expire {
        if exp > expires_at_max {
            exp = expires_at_max;
        };
        expire_date = exp;
    } else {
        expire_date = Timestamp::now().as_u64() as i64
            + Duration::hours(mostro_settings.expiration_hours as i64).num_seconds();
    }
    expire_date
}

#[allow(clippy::too_many_arguments)]
pub async fn publish_order(
    pool: &SqlitePool,
    keys: &Keys,
    new_order: &SmallOrder,
    initiator_pubkey: PublicKey,
    identity_pubkey: PublicKey,
    trade_pubkey: PublicKey,
    request_id: Option<u64>,
    trade_index: Option<i64>,
) -> Result<()> {
    // Prepare a new default order
    let new_order_db = match prepare_new_order(
        new_order,
        initiator_pubkey,
        trade_index,
        identity_pubkey,
        trade_pubkey,
    )
    .await
    {
        Some(order) => order,
        None => {
            return Ok(());
        }
    };

    // CRUD order creation
    let mut order = new_order_db.clone().create(pool).await?;
    let order_id = order.id;
    info!("New order saved Id: {}", order_id);
    // Get user reputation
    let reputation = get_user_reputation(&initiator_pubkey.to_string(), keys).await?;
    // We transform the order fields to tags to use in the event
    let tags = order_to_tags(&new_order_db, reputation);
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
    send_new_order_msg(
        request_id,
        Some(order_id),
        Action::NewOrder,
        Some(Payload::Order(order)),
        &trade_pubkey,
        trade_index,
    )
    .await;

    NOSTR_CLIENT
        .get()
        .unwrap()
        .send_event(event)
        .await
        .map(|_s| ())
        .map_err(|err| err.into())
}

async fn prepare_new_order(
    new_order: &SmallOrder,
    initiator_pubkey: PublicKey,
    trade_index: Option<i64>,
    identity_pubkey: PublicKey,
    trade_pubkey: PublicKey,
) -> Option<Order> {
    let mut fee = 0;
    if new_order.amount > 0 {
        fee = get_fee(new_order.amount);
    }

    // Get expiration time of the order
    let expiry_date = get_expiration_date(new_order.expires_at);

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
        min_amount: new_order.min_amount,
        max_amount: new_order.max_amount,
        fiat_amount: new_order.fiat_amount,
        premium: new_order.premium,
        buyer_invoice: new_order.buyer_invoice.clone(),
        created_at: Timestamp::now().as_u64() as i64,
        expires_at: expiry_date,
        ..Default::default()
    };

    match new_order.kind {
        Some(OrderKind::Buy) => {
            new_order_db.kind = OrderKind::Buy.to_string();
            new_order_db.buyer_pubkey = Some(trade_pubkey.to_string());
            new_order_db.master_buyer_pubkey = Some(identity_pubkey.to_string());
            new_order_db.trade_index_buyer = trade_index;
        }
        Some(OrderKind::Sell) => {
            new_order_db.kind = OrderKind::Sell.to_string();
            new_order_db.seller_pubkey = Some(trade_pubkey.to_string());
            new_order_db.master_seller_pubkey = Some(identity_pubkey.to_string());
            new_order_db.trade_index_seller = trade_index;
        }
        None => {
            send_cant_do_msg(
                None,
                None,
                Some(CantDoReason::InvalidOrderKind),
                &trade_pubkey,
            )
            .await;
            return None;
        }
    }

    // Request price from API in case amount is 0
    new_order_db.price_from_api = new_order.amount == 0;
    Some(new_order_db)
}

pub async fn send_dm(
    receiver_pubkey: &PublicKey,
    sender_keys: Keys,
    payload: String,
    expiration: Option<Timestamp>,
) -> Result<()> {
    info!(
        "sender key {} - receiver key {}",
        sender_keys.public_key().to_hex(),
        receiver_pubkey.to_hex()
    );
    let message = Message::from_json(&payload).unwrap();
    // We sign the message
    let sig = message.get_inner_message_kind().sign(&sender_keys);
    // We compose the content
    let content = (message, sig);
    let content = serde_json::to_string(&content).unwrap();
    // We create the rumor
    let rumor = EventBuilder::text_note(content).build(sender_keys.public_key());
    let mut tags: Vec<Tag> = Vec::with_capacity(1 + usize::from(expiration.is_some()));

    if let Some(timestamp) = expiration {
        tags.push(Tag::expiration(timestamp));
    }
    let tags = Tags::new(tags);

    let event = EventBuilder::gift_wrap(&sender_keys, receiver_pubkey, rumor, tags).await?;
    info!(
        "Sending DM, Event ID: {} with payload: {:#?}",
        event.id, payload
    );

    if let Ok(client) = get_nostr_client() {
        if let Err(e) = client.send_event(event).await {
            error!("Failed to send event: {}", e);
        }
    }

    Ok(())
}

pub fn get_keys() -> Result<Keys> {
    let nostr_settings = Settings::get_nostr();
    // nostr private key
    match Keys::parse(nostr_settings.nsec_privkey) {
        Ok(my_keys) => Ok(my_keys),
        Err(e) => {
            tracing::error!("Failed to parse nostr private key: {}", e);
            std::process::exit(1);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn update_user_rating_event(
    user: &str,
    buyer_sent_rate: bool,
    seller_sent_rate: bool,
    tags: Tags,
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

pub async fn update_order_event(keys: &Keys, status: Status, order: &Order) -> Result<Order> {
    let mut order_updated = order.clone();
    // update order.status with new status
    order_updated.status = status.to_string();
    // We transform the order fields to tags to use in the event
    let tags = order_to_tags(&order_updated, None);
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

    if let Ok(client) = get_nostr_client() {
        if client.send_event(event).await.is_err() {
            tracing::warn!("order id : {} is expired", order_updated.id)
        }
    }

    println!(
        "Inside update_order_event order_updated status {:?} - order id {:?}",
        order_updated.status, order_updated.id,
    );

    Ok(order_updated)
}

pub async fn connect_nostr() -> Result<Client> {
    let nostr_settings = Settings::get_nostr();

    let mut limits = RelayLimits::default();
    // Some specific events can have a bigger size than regular events
    // So we increase the limits for those events
    limits.messages.max_size = Some(6_000);
    limits.events.max_size = Some(6_500);
    let opts = Options::new().relay_limits(limits);

    // Create new client
    let client = ClientBuilder::default().opts(opts).build();

    // Add relays
    for relay in nostr_settings.relays.iter() {
        client.add_relay(relay).await?;
    }

    // Connect to relays and keep connection alive
    client.connect().await;

    Ok(client)
}

pub async fn show_hold_invoice(
    my_keys: &Keys,
    payment_request: Option<String>,
    buyer_pubkey: &PublicKey,
    seller_pubkey: &PublicKey,
    mut order: Order,
    request_id: Option<u64>,
) -> anyhow::Result<()> {
    let mut ln_client = lightning::LndConnector::new().await?;
    // Add fee of seller to hold invoice
    let new_amount = order.amount + order.fee;

    // Now we generate the hold invoice that seller should pay
    let (invoice_response, preimage, hash) = ln_client
        .create_hold_invoice(
            &messages::hold_invoice_description(
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
    let order_updated = update_order_event(my_keys, Status::WaitingPayment, &order).await?;
    order_updated.update(&pool).await?;

    let mut new_order = order.as_new_order();
    new_order.status = Some(Status::WaitingPayment);
    // We create a Message to send the hold invoice to seller
    send_new_order_msg(
        request_id,
        Some(order.id),
        Action::PayInvoice,
        Some(Payload::PaymentRequest(
            Some(new_order),
            invoice_response.payment_request,
            None,
        )),
        seller_pubkey,
        order.trade_index_seller,
    )
    .await;
    // We send a message to buyer to know that seller was requested to pay the invoice
    send_new_order_msg(
        request_id,
        Some(order.id),
        Action::WaitingSellerToPay,
        None,
        buyer_pubkey,
        order.trade_index_buyer,
    )
    .await;

    let _ = invoice_subscribe(hash, request_id).await;

    Ok(())
}

// Create function to reuse in case of resubscription
pub async fn invoice_subscribe(hash: Vec<u8>, request_id: Option<u64>) -> anyhow::Result<()> {
    let mut ln_client_invoices = lightning::LndConnector::new().await?;
    let (tx, mut rx) = channel(100);

    let invoice_task = {
        async move {
            let _ = ln_client_invoices
                .subscribe_invoice(hash, tx)
                .await
                .map_err(|e| MostroError::LnNodeError(e.to_string()));
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
                    if let Err(e) = flow::hold_invoice_paid(&hash, request_id).await {
                        info!("Invoice flow error {e}");
                    } else {
                        info!("Invoice with hash {hash} accepted!");
                    }
                } else if msg.state == InvoiceState::Settled {
                    // If the payment was settled
                    if let Err(e) = flow::hold_invoice_settlement(&hash).await {
                        info!("Invoice flow error {e}");
                    }
                } else if msg.state == InvoiceState::Canceled {
                    // If the payment was canceled
                    if let Err(e) = flow::hold_invoice_canceled(&hash).await {
                        info!("Invoice flow error {e}");
                    }
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
    buyer_pubkey: PublicKey,
    request_id: Option<u64>,
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
        order.min_amount,
        order.max_amount,
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    // We create a Message
    send_new_order_msg(
        request_id,
        Some(order.id),
        Action::AddInvoice,
        Some(Payload::Order(order_data)),
        &buyer_pubkey,
        order.trade_index_buyer,
    )
    .await;

    Ok(order.amount)
}

/// Send message to buyer and seller to vote for counterpart
pub async fn rate_counterpart(
    buyer_pubkey: &PublicKey,
    seller_pubkey: &PublicKey,
    order: &Order,
    request_id: Option<u64>,
) -> Result<()> {
    // Send dm to counterparts
    // to buyer
    send_new_order_msg(
        request_id,
        Some(order.id),
        Action::Rate,
        None,
        buyer_pubkey,
        None,
    )
    .await;
    // to seller
    send_new_order_msg(
        request_id,
        Some(order.id),
        Action::Rate,
        None,
        seller_pubkey,
        None,
    )
    .await;

    Ok(())
}

/// Settle a seller hold invoice
#[allow(clippy::too_many_arguments)]
pub async fn settle_seller_hold_invoice(
    event: &UnwrappedGift,
    ln_client: &mut LndConnector,
    action: Action,
    is_admin: bool,
    order: &Order,
    request_id: Option<u64>,
) -> Result<()> {
    // Check if the pubkey is right
    if !is_admin
        && event.rumor.pubkey.to_string() != *order.seller_pubkey.as_ref().unwrap().to_string()
    {
        send_cant_do_msg(
            request_id,
            Some(order.id),
            Some(CantDoReason::InvalidPubkey),
            &event.rumor.pubkey,
        )
        .await;
        return Err(Error::msg("Not allowed"));
    }

    // Settling the hold invoice
    if let Some(preimage) = order.preimage.as_ref() {
        ln_client.settle_hold_invoice(preimage).await?;
        info!("{action}: Order Id {}: hold invoice settled", order.id);
    } else {
        send_cant_do_msg(
            request_id,
            Some(order.id),
            Some(CantDoReason::InvalidInvoice),
            &event.rumor.pubkey,
        )
        .await;
        return Err(Error::msg("No preimage"));
    }
    Ok(())
}

pub fn bytes_to_string(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut output, b| {
        let _ = write!(output, "{:02x}", b);
        output
    })
}

pub async fn send_cant_do_msg(
    request_id: Option<u64>,
    order_id: Option<Uuid>,
    reason: Option<CantDoReason>,
    destination_key: &PublicKey,
) {
    // Send message to event creator
    let message = Message::cant_do(order_id, request_id, Some(Payload::CantDo(reason)));
    if let Ok(message) = message.as_json() {
        let sender_keys = crate::util::get_keys().unwrap();
        let _ = send_dm(destination_key, sender_keys, message, None).await;
    }
}

pub async fn send_new_order_msg(
    request_id: Option<u64>,
    order_id: Option<Uuid>,
    action: Action,
    payload: Option<Payload>,
    destination_key: &PublicKey,
    trade_index: Option<i64>,
) {
    // Send message to event creator
    let message = Message::new_order(order_id, request_id, trade_index, action, payload);
    if let Ok(message) = message.as_json() {
        let sender_keys = crate::util::get_keys().unwrap();
        let _ = send_dm(destination_key, sender_keys, message, None).await;
    }
}

pub fn get_fiat_amount_requested(order: &Order, msg: &Message) -> Option<i64> {
    // Check if order is range and get amount request after checking boundaries
    // set order fiat amount to the value requested preparing for hold invoice
    if order.is_range_order() {
        if let Some(amount_buyer) = msg.get_inner_message_kind().get_amount() {
            info!("amount_buyer: {amount_buyer}");
            match Some(amount_buyer) <= order.max_amount && Some(amount_buyer) >= order.min_amount {
                true => Some(amount_buyer),
                false => None,
            }
        } else {
            None
        }
    } else {
        // If order is not a range order return an Option with fiat amount of the order
        Some(order.fiat_amount)
    }
}

/// Getter function with error management for nostr Client
pub fn get_nostr_client() -> Result<&'static Client> {
    if let Some(client) = NOSTR_CLIENT.get() {
        Ok(client)
    } else {
        Err(Error::msg("Client not initialized!"))
    }
}

/// Getter function with error management for nostr relays
pub async fn get_nostr_relays() -> Option<HashMap<RelayUrl, Relay>> {
    if let Some(client) = NOSTR_CLIENT.get() {
        Some(client.relays().await)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::message::{Message, MessageKind};
    use mostro_core::order::Order;
    use std::sync::Once;
    use uuid::uuid;
    // Setup function to initialize common settings or data before tests
    static INIT: Once = Once::new();

    fn initialize() {
        INIT.call_once(|| {
            // Any initialization code goes here
        });
    }

    #[test]
    fn test_bytes_to_string() {
        initialize();
        let bytes = vec![0xde, 0xad, 0xbe, 0xef];
        let result = bytes_to_string(&bytes);
        assert_eq!(result, "deadbeef");
    }

    #[tokio::test]
    async fn test_get_market_quote() {
        initialize();
        // Mock the get_market_quote function's external API call
        let fiat_amount = 1000; // $1000
        let fiat_code = "USD";
        let premium = 0;

        // Assuming you have a way to mock the API response
        let sats = get_market_quote(&fiat_amount, fiat_code, premium)
            .await
            .unwrap();
        // Check that sats amount is calculated correctly
        assert!(sats > 0);
    }

    #[tokio::test]
    async fn test_get_nostr_client_failure() {
        initialize();
        // Ensure NOSTR_CLIENT is not initialized for the test
        let client = NOSTR_CLIENT.get();
        assert!(client.is_none());
    }

    #[tokio::test]
    async fn test_get_nostr_client_success() {
        initialize();
        // Mock NOSTR_CLIENT initialization
        let client = Client::default();
        NOSTR_CLIENT.set(client).unwrap();
        let client_result = get_nostr_client();
        assert!(client_result.is_ok());
    }

    #[test]
    fn test_bytes_to_string_empty() {
        initialize();
        let bytes: Vec<u8> = vec![];
        let result = bytes_to_string(&bytes);
        assert_eq!(result, "");
    }

    #[tokio::test]
    async fn test_send_dm() {
        initialize();
        // Mock the send_dm function
        let receiver_pubkey = Keys::generate().public_key();
        let uuid = uuid!("308e1272-d5f4-47e6-bd97-3504baea9c23");
        let message = Message::Order(MessageKind::new(
            Some(uuid),
            None,
            None,
            Action::FiatSent,
            None,
        ));
        let payload = message.as_json().unwrap();
        let sender_keys = Keys::generate();
        let result = send_dm(&receiver_pubkey, sender_keys, payload, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_fiat_amount_requested() {
        initialize();
        let uuid = uuid!("308e1272-d5f4-47e6-bd97-3504baea9c23");
        let order = Order {
            amount: 1000,
            min_amount: Some(500),
            max_amount: Some(2000),
            ..Default::default()
        };
        let message = Message::Order(MessageKind::new(
            Some(uuid),
            Some(1),
            Some(1),
            Action::TakeSell,
            Some(Payload::Amount(order.amount)),
        ));
        let amount = get_fiat_amount_requested(&order, &message);
        assert_eq!(amount, Some(1000));
    }
}
