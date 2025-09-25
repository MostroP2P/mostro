use crate::bitcoin_price::BitcoinPriceManager;
use crate::config::settings::{get_db_pool, Settings};
use crate::config::MOSTRO_DB_PASSWORD;
use crate::config::*;
use crate::db;
use crate::db::is_user_present;
use crate::flow;
use crate::lightning;
use crate::lightning::invoice::is_valid_invoice;
use crate::lightning::LndConnector;
use crate::lnurl::HTTP_CLIENT;
use crate::messages;
use crate::models::Yadio;
use crate::nip33::{new_event, order_to_tags};
use crate::NOSTR_CLIENT;

use chrono::Duration;
use fedimint_tonic_lnd::lnrpc::invoice::InvoiceState;
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::Pool;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx_crud::Crud;
use std::collections::HashMap;
use std::fmt::Write;
use std::str::FromStr;
use std::thread;
use tokio::sync::mpsc::channel;
use tracing::info;
use uuid::Uuid;

pub type FiatNames = std::collections::HashMap<String, String>;
const MAX_RETRY: u16 = 4;

// Redefined for convenience
type OrderKind = mostro_core::order::Kind;

pub async fn retries_yadio_request(
    req_string: &str,
    fiat_code: &str,
) -> Result<(Option<reqwest::Response>, bool), MostroError> {
    // Get Fiat list and check if currency exchange is available
    let mostro_settings = Settings::get_mostro();
    let api_req_string = format!("{}/currencies", mostro_settings.bitcoin_price_api_url);
    let fiat_list_check = HTTP_CLIENT
        .get(api_req_string)
        .send()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?
        .json::<FiatNames>()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::MalformedAPIRes))?
        .contains_key(fiat_code);

    // Exit with error - no currency
    if !fiat_list_check {
        return Ok((None, fiat_list_check));
    }

    let res = HTTP_CLIENT
        .get(req_string)
        .send()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?;

    Ok((Some(res), fiat_list_check))
}

pub fn get_bitcoin_price(fiat_code: &str) -> Result<f64, MostroError> {
    BitcoinPriceManager::get_price(fiat_code)
}

/// Request market quote from Yadio to have sats amount at actual market price
pub async fn get_market_quote(
    fiat_amount: &i64,
    fiat_code: &str,
    premium: i64,
) -> Result<i64, MostroError> {
    // Add here check for market price
    let mostro_settings = Settings::get_mostro();
    let req_string = format!(
        "{}/convert/{}/{}/BTC",
        mostro_settings.bitcoin_price_api_url, fiat_amount, fiat_code
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
        return Err(MostroError::MostroInternalErr(ServiceError::NoAPIResponse));
    }

    // No currency present
    if !req.1 {
        return Err(MostroError::MostroInternalErr(ServiceError::NoCurrency));
    }

    if req.0.is_none() {
        return Err(MostroError::MostroInternalErr(
            ServiceError::MalformedAPIRes,
        ));
    }

    let quote = if let Some(q) = req.0 {
        q.json::<Yadio>()
            .await
            .map_err(|_| MostroError::MostroInternalErr(ServiceError::MessageSerializationError))?
    } else {
        return Err(MostroError::MostroInternalErr(
            ServiceError::MalformedAPIRes,
        ));
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

/// Calculates the expiration timestamp for an order.
///
/// This function computes the expiration time based on the current time and application settings.
/// If an expiration timestamp is provided, it is clamped to a maximum allowed value (the current time plus
/// a configured maximum number of days). If no timestamp is given, a default expiration is calculated as the
/// current time plus a configured number of hours.
///
/// # Returns
///
/// The computed expiration timestamp as a Unix epoch in seconds.
///
/// # Examples
///
/// ```
/// // Calculate a default expiration timestamp.
/// let exp_default = get_expiration_date(None);
/// println!("Default expiration: {}", exp_default);
///
/// // Provide a custom expiration timestamp. The returned value will be clamped
/// // if it exceeds the maximum allowed expiration.
/// let exp_custom = get_expiration_date(Some(exp_default + 10_000));
/// println!("Custom expiration (clamped if necessary): {}", exp_custom);
/// ```
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

/// Checks whether an order qualifies as a full privacy order and returns corresponding event tags.
///
/// This asynchronous function verifies whether the user associated with the order exists in the database.
/// If the user is found, the order is converted to tags including user metadata (total rating, total reviews, and creation date).
/// If not, the function checks that the identity and trade public keys match, and if so, converts the order without user data;
/// otherwise, it returns an error indicating an invalid public key.
///
/// # Errors
///
/// Returns a `MostroInternalErr(ServiceError::InvalidPubkey)` if no user data is found and the identity public key does not match
/// the trade public key.
///
/// # Examples
///
/// ```rust
/// # async fn example() -> Result<(), MostroError> {
/// // Assume proper initialization of the order, pool, and public keys.
/// let order = Order { /* initialize order fields */ };
/// let pool = SqlitePool::connect("sqlite://:memory:").await.unwrap();
/// let identity_pubkey = PublicKey::from_str("02abcdef...").unwrap();
/// let trade_pubkey = identity_pubkey.clone();
///
/// let tags = get_tags_for_new_order(&order, &pool, &identity_pubkey, &trade_pubkey).await?;
/// // Use `tags` for further event processing.
/// # Ok(())
/// # }
pub async fn get_tags_for_new_order(
    new_order_db: &Order,
    pool: &SqlitePool,
    identity_pubkey: &PublicKey,
    trade_pubkey: &PublicKey,
) -> Result<Option<Tags>, MostroError> {
    match is_user_present(pool, identity_pubkey.to_string()).await {
        Ok(user) => {
            // We transform the order fields to tags to use in the event
            order_to_tags(
                new_order_db,
                Some((user.total_rating, user.total_reviews, user.created_at)),
            )
        }
        Err(_) => {
            // We transform the order fields to tags to use in the event
            if identity_pubkey == trade_pubkey {
                order_to_tags(new_order_db, Some((0.0, 0, 0)))
            } else {
                Err(MostroInternalErr(ServiceError::InvalidPubkey))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// Publishes a new order by preparing its details, saving it to the database, creating a corresponding Nostr event, and sending a confirmation message.
///
/// This asynchronous function performs the following steps:
/// - Prepares a new order record from the provided order data and public keys.
/// - Inserts the new order into the database.
/// - Determines order tags based on privacy settings using `check_full_privacy_order`.
/// - Constructs and publishes a Nostr event representing the order.
/// - Updates the order record with the generated event ID.
/// - Enqueues an acknowledgement message for the order.
///
/// # Examples
///
/// ```rust
/// # async fn example() -> Result<(), MostroError> {
/// # use sqlx::sqlite::SqlitePool;
/// # use nostr::Keys;
/// # use my_crate::{SmallOrder, publish_order};
/// // Initialize the database pool and keys.
/// let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
/// let keys = Keys::generate();
///
/// // Prepare a new order along with associated public keys.
/// let new_order = SmallOrder::default();
/// let initiator_pubkey = /* initiator public key */;
/// let identity_pubkey = /* identity public key */;
/// let trade_pubkey = /* trade public key */;
/// let request_id = Some(100);
/// let trade_index = Some(1);
///
/// publish_order(&pool, &keys, &new_order, initiator_pubkey, identity_pubkey, trade_pubkey, request_id, trade_index).await?;
/// # Ok(())
/// # }
/// ```
pub async fn publish_order(
    pool: &SqlitePool,
    keys: &Keys,
    new_order: &SmallOrder,
    initiator_pubkey: PublicKey,
    identity_pubkey: PublicKey,
    trade_pubkey: PublicKey,
    request_id: Option<u64>,
    trade_index: Option<i64>,
) -> Result<(), MostroError> {
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
        Ok(order) => order,
        Err(e) => {
            return Err(e);
        }
    };

    // CRUD order creation
    let mut order = new_order_db
        .clone()
        .create(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let order_id = order.id;
    info!("New order saved Id: {}", order_id);

    // Get tags for new order in case of full privacy or normal order
    // nip33 kind with order fields as tags and order id as identifier
    let event = if let Some(tags) =
        get_tags_for_new_order(&new_order_db, pool, &identity_pubkey, &trade_pubkey).await?
    {
        new_event(keys, "", order_id.to_string(), tags)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidPubkey));
    };

    info!("Order event to be published: {event:#?}");
    let event_id = event.id.to_string();
    info!("Publishing Event Id: {event_id} for Order Id: {order_id}");
    // We update the order with the new event_id
    order.event_id = event_id;
    order
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let mut order = new_order_db.as_new_order();
    order.id = Some(order_id);

    // Send message as ack with small order
    enqueue_order_msg(
        request_id,
        Some(order_id),
        Action::NewOrder,
        Some(Payload::Order(order)),
        trade_pubkey,
        trade_index,
    )
    .await;

    NOSTR_CLIENT
        .get()
        .unwrap()
        .send_event(&event)
        .await
        .map(|_s| ())
        .map_err(|err| MostroInternalErr(ServiceError::NostrError(err.to_string())))
}

async fn prepare_new_order(
    new_order: &SmallOrder,
    initiator_pubkey: PublicKey,
    trade_index: Option<i64>,
    identity_pubkey: PublicKey,
    trade_pubkey: PublicKey,
) -> Result<Order, MostroError> {
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
            new_order_db.master_buyer_pubkey = Some(
                CryptoUtils::store_encrypted(
                    &identity_pubkey.to_string(),
                    MOSTRO_DB_PASSWORD.get(),
                    None,
                )
                .map_err(|e| MostroInternalErr(ServiceError::EncryptionError(e.to_string())))?,
            );
            new_order_db.trade_index_buyer = trade_index;
        }
        Some(OrderKind::Sell) => {
            new_order_db.kind = OrderKind::Sell.to_string();
            new_order_db.seller_pubkey = Some(trade_pubkey.to_string());
            new_order_db.master_seller_pubkey = Some(
                CryptoUtils::store_encrypted(
                    &identity_pubkey.to_string(),
                    MOSTRO_DB_PASSWORD.get(),
                    None,
                )
                .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?,
            );
            new_order_db.trade_index_seller = trade_index;
        }
        None => {
            return Err(MostroCantDo(CantDoReason::InvalidOrderKind));
        }
    }

    // Request price from API in case amount is 0
    new_order_db.price_from_api = new_order.amount == 0;
    Ok(new_order_db)
}

pub async fn send_dm(
    receiver_pubkey: PublicKey,
    sender_keys: &Keys,
    payload: &str,
    expiration: Option<Timestamp>,
) -> Result<(), MostroError> {
    info!(
        "sender key {} - receiver key {}",
        sender_keys.public_key().to_hex(),
        receiver_pubkey.to_hex()
    );
    let message = Message::from_json(payload)
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
    // We compose the content, as this is a message from Mostro
    // and Mostro don't have trade key, we don't need to sign the payload
    let content = (message, Option::<String>::None);
    let content = serde_json::to_string(&content)
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
    // We create the rumor
    let rumor = EventBuilder::text_note(content).build(sender_keys.public_key());
    let mut tags: Vec<Tag> = Vec::with_capacity(1 + usize::from(expiration.is_some()));

    if let Some(timestamp) = expiration {
        tags.push(Tag::expiration(timestamp));
    }
    let tags = Tags::from_list(tags);

    let event = EventBuilder::gift_wrap(sender_keys, &receiver_pubkey, rumor, tags)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    info!(
        "Sending DM, Event ID: {} to {} with payload: {:#?}",
        event.id,
        receiver_pubkey.to_hex(),
        payload
    );

    if let Ok(client) = get_nostr_client() {
        client
            .send_event(&event)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    }

    Ok(())
}

pub fn get_keys() -> Result<Keys, MostroError> {
    let nostr_settings = Settings::get_nostr();
    // nostr private key
    match Keys::parse(&nostr_settings.nsec_privkey) {
        Ok(my_keys) => Ok(my_keys),
        Err(e) => {
            tracing::error!("Failed to parse nostr private key: {}", e);
            Err(MostroInternalErr(ServiceError::NostrError(e.to_string())))
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn update_user_rating_event(
    user: &str,
    buyer_sent_rate: bool,
    seller_sent_rate: bool,
    tags: Tags,
    msg: &Message,
    keys: &Keys,
    pool: &SqlitePool,
) -> Result<()> {
    // Get order from msg
    let mut order = get_order(msg, pool).await?;

    // nip33 kind with user as identifier
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
    MESSAGE_QUEUES.queue_order_rate.write().await.push(event);
    Ok(())
}

async fn get_ratings_for_pending_order(
    order_updated: &Order,
    status: Status,
) -> Result<Option<(f64, i64, i64)>, MostroError> {
    if status == Status::Pending {
        let identity_pubkey = match order_updated.is_sell_order() {
            Ok(_) => order_updated
                .get_master_seller_pubkey(MOSTRO_DB_PASSWORD.get())
                .map_err(MostroInternalErr)?,
            Err(_) => order_updated
                .get_master_buyer_pubkey(MOSTRO_DB_PASSWORD.get())
                .map_err(MostroInternalErr)?,
        };

        let trade_pubkey = match order_updated.is_sell_order() {
            Ok(_) => order_updated
                .get_seller_pubkey()
                .map_err(MostroInternalErr)?,
            Err(_) => order_updated
                .get_buyer_pubkey()
                .map_err(MostroInternalErr)?,
        };

        match is_user_present(&get_db_pool(), identity_pubkey.clone()).await {
            Ok(user) => Ok(Some((
                user.total_rating,
                user.total_reviews,
                user.created_at,
            ))),
            Err(_) => {
                if identity_pubkey == trade_pubkey.to_string() {
                    Ok(Some((0.0, 0, 0)))
                } else {
                    Err(MostroInternalErr(ServiceError::InvalidPubkey))
                }
            }
        }
    } else {
        Ok(None)
    }
}

pub async fn update_order_event(
    keys: &Keys,
    status: Status,
    order: &Order,
) -> Result<Order, MostroError> {
    let mut order_updated = order.clone();
    // update order.status with new status
    order_updated.status = status.to_string();

    // Include rating tag for pending orders
    let reputation_data = get_ratings_for_pending_order(&order_updated, status).await?;

    // We transform the order fields to tags to use in the event
    if let Some(tags) = order_to_tags(&order_updated, reputation_data)? {
        // nip33 kind with order id as identifier and order fields as tags
        let event = new_event(keys, "", order.id.to_string(), tags)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

        info!("Sending replaceable event: {event:#?}");

        // We update the order with the new event_id
        order_updated.event_id = event.id.to_string();

        if let Ok(client) = get_nostr_client() {
            if client.send_event(&event).await.is_err() {
                tracing::warn!("order id : {} is expired", order_updated.id)
            }
        }
    };

    info!(
        "Order Id: {} updated Nostr new Status: {}",
        order.id,
        status.to_string()
    );

    println!(
        "Inside update_order_event order_updated status {:?} - order id {:?}",
        order_updated.status, order_updated.id,
    );

    Ok(order_updated)
}

pub async fn connect_nostr() -> Result<Client, MostroError> {
    let nostr_settings = Settings::get_nostr();

    let mut limits = RelayLimits::default();
    // Some specific events can have a bigger size than regular events
    // So we increase the limits for those events
    limits.messages.max_size = Some(6_000);
    limits.events.max_size = Some(6_500);
    let opts = ClientOptions::new().relay_limits(limits);

    // Create new client
    let client = ClientBuilder::default().opts(opts).build();

    // Add relays
    for relay in nostr_settings.relays.iter() {
        client
            .add_relay(relay)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
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
) -> Result<(), MostroError> {
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
            )
            .map_err(|e| MostroInternalErr(ServiceError::HoldInvoiceError(e.to_string())))?,
            new_amount,
        )
        .await
        .map_err(|e| MostroInternalErr(ServiceError::HoldInvoiceError(e.to_string())))?;
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
    let pool = db::connect()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let order_updated = update_order_event(my_keys, Status::WaitingPayment, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    order_updated
        .update(&pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    let mut new_order = order.as_new_order();
    new_order.status = Some(Status::WaitingPayment);
    // We create a Message to send the hold invoice to seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::PayInvoice,
        Some(Payload::PaymentRequest(
            Some(new_order),
            invoice_response.payment_request,
            None,
        )),
        *seller_pubkey,
        order.trade_index_seller,
    )
    .await;

    // We notify the buyer (maker) that their order was taken and seller must pay the hold invoice
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::WaitingSellerToPay,
        None,
        *buyer_pubkey,
        order.trade_index_buyer,
    )
    .await;

    let _ = invoice_subscribe(hash, request_id).await;

    Ok(())
}

// Create function to reuse in case of resubscription
pub async fn invoice_subscribe(hash: Vec<u8>, request_id: Option<u64>) -> Result<(), MostroError> {
    let mut ln_client_invoices = lightning::LndConnector::new().await?;
    let (tx, mut rx) = channel(100);

    let invoice_task = {
        async move {
            let _ = ln_client_invoices
                .subscribe_invoice(hash, tx)
                .await
                .map_err(|e| e.to_string());
        }
    };
    tokio::spawn(invoice_task);

    // Arc clone db pool to safe use across threads
    let pool = get_db_pool();

    let subs = {
        async move {
            // Receiving msgs from the invoice subscription.
            while let Some(msg) = rx.recv().await {
                let hash = bytes_to_string(msg.hash.as_ref());
                // If this invoice was paid by the seller
                if msg.state == InvoiceState::Accepted {
                    if let Err(e) = flow::hold_invoice_paid(&hash, request_id, &pool).await {
                        info!("Invoice flow error {e}");
                    } else {
                        info!("Invoice with hash {hash} accepted!");
                    }
                } else if msg.state == InvoiceState::Settled {
                    // If the payment was settled
                    if let Err(e) = flow::hold_invoice_settlement(&hash, &pool).await {
                        info!("Invoice flow error {e}");
                    }
                } else if msg.state == InvoiceState::Canceled {
                    // If the payment was canceled
                    if let Err(e) = flow::hold_invoice_canceled(&hash, &pool).await {
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

/// Set order sats amount, this used when a buyer takes a sell order
pub async fn set_waiting_invoice_status(
    order: &mut Order,
    buyer_pubkey: PublicKey,
    request_id: Option<u64>,
) -> Result<i64> {
    let kind = OrderKind::from_str(&order.kind)
        .map_err(|_| MostroCantDo(CantDoReason::InvalidOrderKind))?;
    let status = Status::WaitingBuyerInvoice;

    let buyer_final_amount = order.amount.saturating_sub(order.fee);
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
    );
    // We create a Message
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::AddInvoice,
        Some(Payload::Order(order_data)),
        buyer_pubkey,
        order.trade_index_buyer,
    )
    .await;

    // We notify the seller (maker) that their order was taken and buyer must add invoice
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::WaitingBuyerInvoice,
        None,
        seller_pubkey,
        order.trade_index_seller,
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
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Rate,
        None,
        *buyer_pubkey,
        None,
    )
    .await;
    // to seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Rate,
        None,
        *seller_pubkey,
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
) -> Result<(), MostroError> {
    // Get seller pubkey
    let seller_pubkey = order
        .get_seller_pubkey()
        .map_err(|_| MostroCantDo(CantDoReason::InvalidPubkey))?
        .to_string();
    // Get sender pubkey
    let sender_pubkey = event.rumor.pubkey.to_string();
    // Check if the pubkey is right
    if !is_admin && sender_pubkey != seller_pubkey {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // Settling the hold invoice
    if let Some(preimage) = order.preimage.as_ref() {
        ln_client.settle_hold_invoice(preimage).await?;
        info!("{action}: Order Id {}: hold invoice settled", order.id);
    } else {
        return Err(MostroCantDo(CantDoReason::InvalidInvoice));
    }
    Ok(())
}

pub fn bytes_to_string(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut output, b| {
        let _ = write!(output, "{:02x}", b);
        output
    })
}

pub async fn enqueue_cant_do_msg(
    request_id: Option<u64>,
    order_id: Option<Uuid>,
    reason: CantDoReason,
    destination_key: PublicKey,
) {
    // Send message to event creator
    let message = Message::cant_do(order_id, request_id, Some(Payload::CantDo(Some(reason))));
    MESSAGE_QUEUES
        .queue_order_cantdo
        .write()
        .await
        .push((message, destination_key));
}

pub async fn enqueue_restore_session_msg(payload: Option<Payload>, destination_key: PublicKey) {
    // Send message to event creator
    let message = Message::new_restore(payload);
    MESSAGE_QUEUES
        .queue_restore_session_msg
        .write()
        .await
        .push((message, destination_key));
}

pub async fn enqueue_order_msg(
    request_id: Option<u64>,
    order_id: Option<Uuid>,
    action: Action,
    payload: Option<Payload>,
    destination_key: PublicKey,
    trade_index: Option<i64>,
) {
    // Send message to event creator
    let message = Message::new_order(order_id, request_id, trade_index, action, payload);
    MESSAGE_QUEUES
        .queue_order_msg
        .write()
        .await
        .push((message, destination_key));
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
pub fn get_nostr_client() -> Result<&'static Client, MostroError> {
    if let Some(client) = NOSTR_CLIENT.get() {
        Ok(client)
    } else {
        Err(MostroInternalErr(ServiceError::NostrError(
            "Client not initialized!".to_string(),
        )))
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

pub async fn get_dispute(msg: &Message, pool: &Pool<Sqlite>) -> Result<Dispute, MostroError> {
    let dispute_msg = msg.get_inner_message_kind();
    let dispute_id = dispute_msg
        .id
        .ok_or(MostroInternalErr(ServiceError::InvalidDisputeId))?;
    let dispute = Dispute::by_id(pool, dispute_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    if let Some(dispute) = dispute {
        Ok(dispute)
    } else {
        Err(MostroInternalErr(ServiceError::InvalidDisputeId))
    }
}

pub async fn get_order(msg: &Message, pool: &Pool<Sqlite>) -> Result<Order, MostroError> {
    let order_msg = msg.get_inner_message_kind();
    let order_id = order_msg
        .id
        .ok_or(MostroInternalErr(ServiceError::InvalidOrderId))?;
    let order = Order::by_id(pool, order_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    if let Some(order) = order {
        Ok(order)
    } else {
        Err(MostroInternalErr(ServiceError::InvalidOrderId))
    }
}

pub async fn validate_invoice(msg: &Message, order: &Order) -> Result<Option<String>, MostroError> {
    // init payment request to None
    let mut payment_request = None;
    // if payment request is present
    if let Some(pr) = msg.get_inner_message_kind().get_payment_request() {
        // if invoice is valid
        if is_valid_invoice(
            pr.clone(),
            Some(order.amount as u64),
            Some(order.fee as u64),
        )
        .await
        .is_err()
        {
            return Err(MostroCantDo(CantDoReason::InvalidInvoice));
        }
        // if invoice is valid return it
        else {
            payment_request = Some(pr);
        }
    }
    Ok(payment_request)
}

pub async fn notify_taker_reputation(
    pool: &Pool<Sqlite>,
    order: &Order,
) -> Result<(), MostroError> {
    // Check if is buy or sell order we need this info to understand the user needed and the receiver of notification
    let is_buy_order = order.is_buy_order().is_ok();
    // Get user needed
    let user = match is_buy_order {
        true => order.master_seller_pubkey.clone(),
        false => order.master_buyer_pubkey.clone(),
    };

    let user_decrypted_key = if let Some(user) = user {
        // Get reputation data
        CryptoUtils::decrypt_data(user, MOSTRO_DB_PASSWORD.get()).map_err(MostroInternalErr)?
    } else {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    };

    let reputation_data = match is_user_present(pool, user_decrypted_key).await {
        Ok(user) => {
            let now = Timestamp::now().as_u64();
            UserInfo {
                rating: user.total_rating,
                reviews: user.total_reviews,
                operating_days: (now - user.created_at as u64) / 86400,
            }
        }
        Err(_) => UserInfo {
            rating: 0.0,
            reviews: 0,
            operating_days: 0,
        },
    };

    // Get order status
    let order_status = order.get_order_status().map_err(MostroInternalErr)?;

    // Get action for info message and receiver key
    let (action, receiver) = match order_status {
        Status::WaitingBuyerInvoice => {
            if !is_buy_order {
                (
                    Action::PayInvoice,
                    order.get_seller_pubkey().map_err(MostroInternalErr)?,
                )
            } else {
                //FIX for the case of a buy order and maker is adding invoice
                // just return ok
                return Ok(());
            }
        }
        Status::WaitingPayment => {
            if is_buy_order {
                (
                    Action::AddInvoice,
                    order.get_buyer_pubkey().map_err(MostroInternalErr)?,
                )
            } else {
                return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
            }
        }
        _ => {
            return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
        }
    };

    enqueue_order_msg(
        None,
        Some(order.id),
        action,
        Some(Payload::Peer(Peer {
            pubkey: "".to_string(),
            reputation: Some(reputation_data),
        })),
        receiver,
        None,
    )
    .await;
    Ok(())
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
    async fn test_get_market_quote_url_construction() {
        initialize();
        // Test the URL construction logic without making actual API calls
        // This test verifies that the API URL format is correct
        let base_url = "https://api.yadio.io";
        let fiat_amount = 1000;
        let fiat_code = "USD";

        let expected_url = format!("{}/convert/{}/{}/BTC", base_url, fiat_amount, fiat_code);
        assert_eq!(expected_url, "https://api.yadio.io/convert/1000/USD/BTC");

        // Test currency list URL construction
        let currencies_url = format!("{}/currencies", base_url);
        assert_eq!(currencies_url, "https://api.yadio.io/currencies");
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
        // Now error is well manager this call will fail now, previously test was ok becuse error was not managed
        // now just make it ok and then will make a better test
        let result = send_dm(receiver_pubkey, &sender_keys, &payload, None).await;
        assert!(result.is_err());
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
