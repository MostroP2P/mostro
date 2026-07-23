use crate::config::constants::{
    DEV_FEE_AUDIT_EVENT_KIND, DEV_FEE_LIGHTNING_ADDRESS, DM_EVENT_KIND,
};
use crate::config::settings::{get_db_pool, Settings};
use crate::config::*;
use crate::db;
use crate::db::is_user_present;
use crate::escrow::EscrowBackend;
use crate::flow;
use crate::lightning;
use crate::lightning::invoice::is_valid_invoice;
use crate::messages;
use crate::nip33::{create_platform_tag_values, new_order_event, new_rating_event, order_to_tags};
use crate::NOSTR_CLIENT;

use chrono::Duration;
use fedimint_tonic_lnd::lnrpc::invoice::InvoiceState;
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::Pool;
use sqlx::QueryBuilder;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::fmt::Write;
use std::str::FromStr;
use tokio::sync::mpsc::channel;
use tracing::info;
use uuid::Uuid;

// Redefined for convenience
type OrderKind = mostro_core::order::Kind;

pub fn get_bitcoin_price(fiat_code: &str) -> Result<f64, MostroError> {
    crate::price::get_bitcoin_price(fiat_code)
}

/// Convert a fiat amount to sats at the current market rate, applying the
/// order premium.
///
/// Phase 4 (spec §6.4, §9 Phase 4): reads the aggregated, multi-source rate
/// from the in-memory price store instead of making a live Yadio `/convert`
/// call per take. The store is refreshed by the scheduler tick, so this is a
/// lock-read with no network I/O. Staleness is enforced upstream by
/// [`crate::price::get_bitcoin_price`]: a rate older than the configured
/// window returns `ServiceError::PriceTooStale` rather than pricing on stale
/// data, which order create/take surface to the user as
/// `CantDoReason::PriceTooStale`.
pub fn get_market_quote(
    fiat_amount: &i64,
    fiat_code: &str,
    premium: i64,
) -> Result<i64, MostroError> {
    // Fiat units per 1 BTC (staleness-enforced).
    let price = get_bitcoin_price(fiat_code)?;
    if price <= 0.0 {
        return Err(MostroError::MostroInternalErr(ServiceError::NoAPIResponse));
    }

    // sats = (fiat_amount / fiat_per_btc) × 1e8.
    let mut sats = (*fiat_amount as f64 / price) * 100_000_000_f64;

    // Apply the order premium to the sats value.
    if premium != 0 {
        sats -= (premium as f64) / 100_f64 * sats;
    }

    Ok(sats as i64)
}

pub fn get_fee(amount: i64) -> i64 {
    let mostro_settings = Settings::get_mostro();
    // We calculate the bot fee
    let split_fee = (mostro_settings.fee * amount as f64) / 2.0;
    split_fee.round() as i64
}

/// Calculates the development fee as a percentage of the total Mostro fee.
///
/// This is a pure function that performs the fee calculation without accessing global state.
/// Useful for testing with different percentage values.
///
/// # Arguments
/// * `total_mostro_fee` - The total Mostro fee amount in satoshis
/// * `percentage` - The percentage to apply (e.g., 0.30 for 30%)
///
/// # Returns
/// The calculated development fee, rounded to nearest satoshi
pub fn calculate_dev_fee(total_mostro_fee: i64, percentage: f64) -> i64 {
    let dev_fee = (total_mostro_fee as f64) * percentage;
    dev_fee.round() as i64
}

/// Calculate total development fee from the total Mostro fee
/// Takes the TOTAL Mostro fee (both parties combined) and returns the TOTAL dev fee
/// The returned value should be split 50/50 between buyer and seller
/// Returns the total amount in satoshis for the dev fund
pub fn get_dev_fee(total_mostro_fee: i64) -> i64 {
    let mostro_settings = Settings::get_mostro();
    calculate_dev_fee(total_mostro_fee, mostro_settings.dev_fee_percentage)
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
    let expires_at_max: i64 = Timestamp::now().as_secs() as i64
        + Duration::days(mostro_settings.max_expiration_days.into()).num_seconds();
    if let Some(mut exp) = expire {
        if exp > expires_at_max {
            exp = expires_at_max;
        };
        expire_date = exp;
    } else {
        expire_date = Timestamp::now().as_secs() as i64
            + Duration::hours(mostro_settings.expiration_hours as i64).num_seconds();
    }
    expire_date
}

/// Get expiration timestamp for an event kind based on expiration configuration
///
/// This function calculates the expiration timestamp for different event kinds
/// using the configured expiration days per kind. Falls back to max_expiration_days
/// if no expiration configuration is available (backward compatibility).
///
/// # Arguments
///
/// * `kind` - The event kind (38383 for orders, 38384 for ratings, 38386 for disputes, 8383 for fee audits)
///
/// # Returns
///
/// * `Some(i64)` - Unix timestamp when the event should expire
/// * `None` - If the event kind should not have expiration
///
/// # Examples
///
/// ```
/// // Get expiration for a dispute event (kind 38386)  
/// let dispute_expiration = get_expiration_timestamp_for_kind(38386);
/// ```
pub fn get_expiration_timestamp_for_kind(kind: u16) -> Option<i64> {
    let now = Timestamp::now().as_secs() as i64;

    // Try to get expiration from new configuration first
    if let Some(exp_config) = Settings::get_expiration() {
        if let Some(days) = exp_config.get_expiration_for_kind(kind) {
            return Some(now + Duration::days(days as i64).num_seconds());
        }
    }

    // Backward-compat fallback for known kinds only.
    // Keep this list in sync with `ExpirationSettings::get_expiration_for_kind` in `src/config/types.rs`
    // when adding/removing event kinds.
    match kind {
        NOSTR_ORDER_EVENT_KIND
        | NOSTR_RATING_EVENT_KIND
        | NOSTR_DISPUTE_EVENT_KIND
        | DEV_FEE_AUDIT_EVENT_KIND => {
            let mostro_settings = Settings::get_mostro();
            Some(now + Duration::days(mostro_settings.max_expiration_days.into()).num_seconds())
        }
        // Protocol-v2 direct messages: same 30-day default as
        // `ExpirationSettings::get_expiration_for_kind`.
        DM_EVENT_KIND => Some(now + Duration::days(30).num_seconds()),
        _ => None,
    }
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
/// let tags = get_tags_for_new_order(&order, &pool, &identity_pubkey, &trade_pubkey, &keys).await?;
/// // Use `tags` for further event processing.
/// # Ok(())
/// # }
pub async fn get_tags_for_new_order(
    new_order_db: &Order,
    pool: &SqlitePool,
    identity_pubkey: &PublicKey,
    trade_pubkey: &PublicKey,
    mostro_keys: &Keys,
) -> Result<Option<Tags>, MostroError> {
    let mostro_pubkey = mostro_keys.public_key().to_hex();
    match is_user_present(pool, identity_pubkey.to_string()).await {
        Ok(user) => {
            // We transform the order fields to tags to use in the event
            order_to_tags(
                new_order_db,
                Some((user.total_rating, user.total_reviews, user.created_at)),
                Some(&mostro_pubkey),
            )
        }
        Err(_) => {
            // We transform the order fields to tags to use in the event
            if identity_pubkey == trade_pubkey {
                order_to_tags(new_order_db, Some((0.0, 0, 0)), Some(&mostro_pubkey))
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
    let mut new_order_db = match prepare_new_order(
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

    // Phase 5/6: when the maker side is bonded, the order must NOT hit the
    // order book until the maker locks an anti-abuse bond. Park it at
    // `WaitingMakerBond` (no NIP-33 event emitted), request the bond, and
    // defer the publication to `resume_publish_after_maker_bond`, which
    // the bond subscriber calls on `Accepted`. Both fixed-amount (Phase 5)
    // and range (Phase 6) orders take this path; range orders size the
    // bond against `max_amount` (worst-case exposure) and resolve slashes
    // proportionally per taken slice — see `maker_bond_notional_sats`.
    let maker_bond_required = crate::app::bond::maker_bond_required();
    if maker_bond_required {
        let notional = maker_bond_notional_sats(&new_order_db)?;
        new_order_db.status = Status::WaitingMakerBond.to_string();
        let order = new_order_db
            .create(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        info!("New order saved (awaiting maker bond) Id: {}", order.id);
        if let Err(e) = crate::app::bond::request_maker_bond(
            pool,
            &order,
            trade_pubkey,
            notional,
            request_id,
            trade_index,
        )
        .await
        {
            // The order was parked at `WaitingMakerBond` but never emitted
            // a NIP-33 event, and `request_maker_bond` already released any
            // bond row it managed to create. Without cleanup the row would
            // sit hidden in `WaitingMakerBond` until the order-expiry job
            // reaps it hours later. Delete the stranded row now (scoped to
            // the parked status so we never touch one that has since
            // advanced) and surface the error to the maker.
            tracing::warn!(
                order_id = %order.id,
                "publish_order: request_maker_bond failed ({}); deleting stranded WaitingMakerBond order",
                e
            );
            if let Err(del) = sqlx::query("DELETE FROM orders WHERE id = ? AND status = ?")
                .bind(order.id)
                .bind(Status::WaitingMakerBond.to_string())
                .execute(pool)
                .await
            {
                tracing::warn!(
                    order_id = %order.id,
                    "publish_order: failed to delete stranded order: {}", del
                );
            }
            return Err(e);
        }
        return Ok(());
    }

    // CRUD order creation
    let order = new_order_db
        .create(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    info!("New order saved Id: {}", order.id);

    finalize_order_publication(
        pool,
        keys,
        order,
        identity_pubkey,
        trade_pubkey,
        request_id,
        trade_index,
    )
    .await
}

/// Sats notional a maker bond is sized against.
///
/// - **Range orders (Phase 6).** Sized against `max_amount` — the
///   worst-case fiat exposure the maker is advertising — converted at the
///   current cached price. Each taken slice later slashes a proportional
///   share of the resulting bond (`slice.fiat_amount / max_amount`), so
///   the notional must be the range ceiling, not any single slice.
/// - **Fixed-price orders.** Carry their sats `amount` directly.
/// - **Market-priced single orders.** `amount == 0` at creation, so we
///   convert the fiat amount at the current cached price — the same quote
///   `calculate_and_check_quote` validates against at order time.
///
/// The bond is a one-time snapshot and is not repriced if the market moves
/// before the order is taken (spec §10.3).
fn maker_bond_notional_sats(order: &Order) -> Result<i64, MostroError> {
    // Range orders: size against the fiat ceiling (`max_amount`).
    if order.is_range_order() {
        // `is_range_order()` only checks that `min`/`max` are `Some`, not
        // that they are positive, so guard against a zero/negative ceiling
        // here — a non-positive `max_amount` would otherwise size the bond
        // at the floor and later divide-by-zero in the proportional slash
        // (`record_maker_slice_slash`, which carries the matching guard).
        let max_fiat = order.max_amount.filter(|m| *m > 0).ok_or_else(|| {
            MostroInternalErr(ServiceError::UnexpectedError(
                "range order missing positive max_amount".to_string(),
            ))
        })?;
        let price = get_bitcoin_price(&order.fiat_code)?;
        if price <= 0.0 {
            return Err(MostroInternalErr(ServiceError::NoAPIResponse));
        }
        let sats = (max_fiat as f64 / price) * 1E8;
        return Ok(sats as i64);
    }
    if order.amount > 0 {
        return Ok(order.amount);
    }
    let price = get_bitcoin_price(&order.fiat_code)?;
    if price <= 0.0 {
        return Err(MostroInternalErr(ServiceError::NoAPIResponse));
    }
    let sats = (order.fiat_amount as f64 / price) * 1E8;
    Ok(sats as i64)
}

/// Publish the NIP-33 event for a freshly-persisted order, persist its
/// `event_id`, ack the maker with [`Action::NewOrder`], and broadcast.
///
/// Shared by the inline `publish_order` path (no maker bond) and the
/// deferred [`resume_publish_after_maker_bond`] path (maker bond locked).
/// The order row must already exist in the DB; on success it is in
/// `Status::Pending` with its `event_id` set.
async fn finalize_order_publication(
    pool: &SqlitePool,
    keys: &Keys,
    mut order: Order,
    identity_pubkey: PublicKey,
    trade_pubkey: PublicKey,
    request_id: Option<u64>,
    trade_index: Option<i64>,
) -> Result<(), MostroError> {
    let order_id = order.id;
    // The maker-bond path parked the order at `WaitingMakerBond`; the
    // no-bond path created it at `Pending`. Either way it goes live now.
    order.status = Status::Pending.to_string();

    // Get tags for new order in case of full privacy or normal order
    // nip33 kind with order fields as tags and order id as identifier (kind 38383 for orders)
    let event = if let Some(tags) =
        get_tags_for_new_order(&order, pool, &identity_pubkey, &trade_pubkey, keys).await?
    {
        new_order_event(keys, "", order_id.to_string(), tags)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidPubkey));
    };

    info!("Order event to be published: {event:#?}");
    let event_id = event.id.to_string();
    info!("Publishing Event Id: {event_id} for Order Id: {order_id}");
    // We update the order with the new event_id (and Pending status)
    order.event_id = event_id;
    // Build the ack payload before `update` consumes the order row.
    let mut small = order.as_new_order();
    small.id = Some(order_id);
    order
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Send message as ack with small order
    enqueue_order_msg(
        request_id,
        Some(order_id),
        Action::NewOrder,
        Some(Payload::Order(small)),
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

/// Finish publishing an order whose maker bond has just locked.
///
/// Called from the bond subscriber (`bond::flow::on_maker_bond_accepted`).
/// Derives the maker's identity and trade pubkeys from the order row —
/// the maker is the seller on a sell order, the buyer on a buy order
/// (§3.1) — and hands off to [`finalize_order_publication`]. Idempotency
/// across redeliveries is enforced by the caller, which only invokes this
/// while the order is still in `WaitingMakerBond`.
pub async fn resume_publish_after_maker_bond(
    pool: &SqlitePool,
    keys: &Keys,
    order: Order,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    // Atomically claim the deferred `WaitingMakerBond → Pending`
    // transition. The bond subscriber already re-read the row and saw
    // `WaitingMakerBond`, but that check is not atomic with the publish
    // below: the order-expiry job (`job_expire_pending_older_orders`) can
    // flip the row `WaitingMakerBond → Expired` (and cancel the just-locked
    // bond) in between. Without a CAS the full-row write inside
    // `finalize_order_publication` would blindly resurrect the dead order
    // back to `Pending` and emit a NIP-33 event for it. If the CAS affects
    // 0 rows another path already owns the status, so we skip cleanly.
    let cas = sqlx::query("UPDATE orders SET status = ? WHERE id = ? AND status = ?")
        .bind(Status::Pending.to_string())
        .bind(order.id)
        .bind(Status::WaitingMakerBond.to_string())
        .execute(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    if cas.rows_affected() != 1 {
        info!(
            "resume_publish_after_maker_bond: order {} no longer WaitingMakerBond — skipping deferred publish",
            order.id
        );
        return Ok(());
    }
    let kind = order.get_order_kind().map_err(MostroInternalErr)?;
    let (trade_pubkey, identity_pubkey, trade_index) = match kind {
        OrderKind::Sell => (
            order.get_seller_pubkey().map_err(MostroInternalErr)?,
            order
                .get_master_seller_pubkey()
                .map_err(MostroInternalErr)?,
            order.trade_index_seller,
        ),
        OrderKind::Buy => (
            order.get_buyer_pubkey().map_err(MostroInternalErr)?,
            order.get_master_buyer_pubkey().map_err(MostroInternalErr)?,
            order.trade_index_buyer,
        ),
    };
    finalize_order_publication(
        pool,
        keys,
        order,
        identity_pubkey,
        trade_pubkey,
        request_id,
        trade_index,
    )
    .await
}

async fn prepare_new_order(
    new_order: &SmallOrder,
    initiator_pubkey: PublicKey,
    trade_index: Option<i64>,
    identity_pubkey: PublicKey,
    trade_pubkey: PublicKey,
) -> Result<Order, MostroError> {
    let mut fee = 0;
    // dev_fee is always calculated when the order is taken, not at creation time
    // This unifies the behavior for both fixed price and market price orders
    let dev_fee = 0;
    if new_order.amount > 0 {
        fee = get_fee(new_order.amount); // Get split fee (each party's share)
                                         // dev_fee will be calculated in take_buy_action() or take_sell_action()
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
        dev_fee,
        dev_fee_paid: false,
        dev_fee_payment_hash: None,
        fiat_code: new_order.fiat_code.clone(),
        min_amount: new_order.min_amount,
        max_amount: new_order.max_amount,
        fiat_amount: new_order.fiat_amount,
        premium: new_order.premium,
        buyer_invoice: new_order.buyer_invoice.clone(),
        created_at: Timestamp::now().as_secs() as i64,
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
            return Err(MostroCantDo(CantDoReason::InvalidOrderKind));
        }
    }

    // Request price from API in case amount is 0
    new_order_db.price_from_api = new_order.amount == 0;
    Ok(new_order_db)
}

/// Overwrite the inner protocol version of `message` so it matches the wire
/// `transport` (`gift-wrap` -> v1, `nip44` -> v2).
///
/// `MessageKind::new` always stamps the crate-wide `PROTOCOL_VER`, so without
/// this every reply would advertise v2 even when it is served over the v1
/// gift-wrap transport. Keeping the inner version aligned with the transport
/// lets the protocol version follow the negotiated wire format.
///
/// DEPRECATED(v0.19.0, #786): transitional mechanism from PR #785. v0.19.0
/// runs protocol v2 only, the inner version becomes the `PROTOCOL_VER`
/// constant again and this function is deleted.
#[deprecated(
    since = "0.18.0",
    note = "transitional version-follows-transport stamping; removed in v0.19.0 (protocol v2 only) — see issue #786"
)]
fn stamp_protocol_version(message: &mut Message, transport: Transport) {
    let version = transport.protocol_version();
    match message {
        Message::Order(k)
        | Message::Dispute(k)
        | Message::CantDo(k)
        | Message::Rate(k)
        | Message::Dm(k)
        | Message::Restore(k) => k.version = version,
    }
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
    let mut message = Message::from_json(payload)
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;

    // Non-panicking accessor: send_dm sits on every reply path and is
    // exercised by unit tests that don't initialize the global config.
    // DEPRECATED(v0.19.0, #786): both calls below go away with the
    // `transport` setting.
    #[allow(deprecated)]
    let transport = Settings::get_transport();

    // Stamp the inner protocol version to match the active wire transport.
    // Done before wrapping so the version is covered by the message/trade
    // signatures.
    #[allow(deprecated)]
    stamp_protocol_version(&mut message, transport);

    // Kind-14 events are visible to relays, so they always carry a NIP-40
    // expiration tag (default 30 days via `dm_days`) instead of lingering
    // forever. Callers that pass an explicit expiration keep it.
    let expiration = match (transport, expiration) {
        (Transport::Nip44Direct, None) => get_expiration_timestamp_for_kind(DM_EVENT_KIND)
            .map(|secs| Timestamp::from_secs(secs as u64)),
        (_, exp) => exp,
    };

    // Mostro node holds a single keypair: it doubles as identity and trade key.
    // Server-originated messages are unsigned because clients don't track a
    // trade_index for the node.
    let event = wrap_message_with(
        transport,
        &message,
        sender_keys,
        sender_keys,
        receiver_pubkey,
        WrapOptions {
            signed: false,
            expiration,
            ..WrapOptions::default()
        },
    )
    .await?;

    info!(
        "Sending message, Event ID: {} to {} with payload: {:#?}",
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

/// Publishes a dev fee payment audit event to Nostr relays
///
/// This function creates and publishes a Nostr event (kind 8383) containing
/// audit information about a successful dev fee payment. The event includes
/// payment details for transparency and third-party verification.
///
/// # Arguments
/// * `order` - The order for which dev fee was paid
/// * `payment_hash` - The Lightning Network payment hash
///
/// # Returns
/// * `Ok(())` if event was published successfully
/// * `Err(MostroError)` if publishing failed
///
/// # Privacy
/// This function does NOT include buyer or seller pubkeys to maintain user privacy.
/// Only aggregate payment data and order metadata are published.
pub async fn publish_dev_fee_audit_event(
    order: &Order,
    payment_hash: &str,
) -> Result<(), MostroError> {
    use std::borrow::Cow;
    let ln_network = match LN_STATUS.get() {
        Some(status) => status.networks.join(","),
        None => "unknown".to_string(),
    };
    // Get Mostro keys for signing
    let keys = get_keys()?;

    // Get Nostr client
    let client = get_nostr_client()?;

    // Create tags for queryability
    let mut tag_list = vec![
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("order-id")),
            vec![order.id.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("amount")),
            vec![order.dev_fee.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("hash")),
            vec![payment_hash.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("destination")),
            vec![DEV_FEE_LIGHTNING_ADDRESS.to_string()],
        ),
        Tag::custom(TagKind::Custom(Cow::Borrowed("network")), vec![ln_network]),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("y")),
            create_platform_tag_values(Settings::get_mostro().name.as_deref()),
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("z")),
            vec!["dev-fee-payment".to_string()],
        ),
    ];

    // Add expiration tag if configured
    if let Some(expiration_timestamp) = get_expiration_timestamp_for_kind(DEV_FEE_AUDIT_EVENT_KIND)
    {
        tag_list.push(Tag::expiration(Timestamp::from(
            expiration_timestamp as u64,
        )));
    }

    let tags = Tags::from_list(tag_list);

    // Create and sign event
    let event = EventBuilder::new(nostr_sdk::Kind::Custom(DEV_FEE_AUDIT_EVENT_KIND), "")
        .tags(tags)
        .sign_with_keys(&keys)
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Publish event to relays
    client
        .send_event(&event)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    info!(
        "📡 Published dev fee audit event for order {} - {} sats to relays",
        order.id, order.dev_fee
    );

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

    // nip33 kind with user as identifier (kind 38384 for ratings)
    let event = new_rating_event(keys, "", user.to_string(), tags)?;
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
    // Phase 1.5: `WaitingTakerBond` publishes on the wire as `pending`
    // (see `nip33::create_status_tags`), so the maker rating must travel
    // with both buckets — otherwise clients browsing the orderbook would
    // see the order without ratings during the bond window.
    if status == Status::Pending || status == Status::WaitingTakerBond {
        let identity_pubkey = match order_updated.is_sell_order() {
            Ok(_) => order_updated
                .get_master_seller_pubkey()
                .map_err(MostroInternalErr)?,
            Err(_) => order_updated
                .get_master_buyer_pubkey()
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

        match is_user_present(&get_db_pool(), identity_pubkey.to_string()).await {
            Ok(user) => Ok(Some((
                user.total_rating,
                user.total_reviews,
                user.created_at,
            ))),
            Err(_) => {
                if identity_pubkey == trade_pubkey {
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
    let mostro_pubkey = keys.public_key().to_hex();
    if let Some(tags) = order_to_tags(&order_updated, reputation_data, Some(&mostro_pubkey))? {
        // nip33 kind with order id as identifier and order fields as tags (kind 38383 for orders)
        let event = new_order_event(keys, "", order.id.to_string(), tags)
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
    // Seller pays only the order amount and their Mostro fee
    // Dev fee is NOT charged to seller - it's paid by mostrod from its earnings
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
    new_order.amount = new_amount;
    // Clear buyer_invoice to avoid leaking buyer's payment info to seller
    new_order.buyer_invoice = None;

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
                    let keys = match get_keys() {
                        Ok(k) => k,
                        Err(e) => {
                            info!("Failed to get keys: {e}");
                            continue;
                        }
                    };
                    if let Err(e) = flow::hold_invoice_paid(&hash, request_id, &pool, &keys).await {
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

/// Price a market order and compute its Mostro fee in one step.
///
/// Converts `fiat_amount` (denominated in `fiat_code`) to sats through the
/// cache-backed [`get_market_quote`], applying `premium`, then derives the
/// Mostro fee from that amount. Returns `(sats_amount, fee)` — the order
/// amount in sats first, the fee in sats second. Errors bubble up from the
/// quote path: `PriceTooStale` when the cached rate is past the staleness
/// window, `NoAPIResponse` when the currency has no cached price yet.
pub fn get_market_amount_and_fee(
    fiat_amount: i64,
    fiat_code: &str,
    premium: i64,
) -> Result<(i64, i64), MostroError> {
    // Update amount order
    let new_sats_amount = get_market_quote(&fiat_amount, fiat_code, premium)?;
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

    // Buyer receives order amount minus only the Mostro fee
    // Dev fee is NOT charged to buyer - it's paid by mostrod from its earnings
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
        Some(order.created_at),
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
    event: &UnwrappedMessage,
    escrow: &mut dyn EscrowBackend,
    action: Action,
    is_admin: bool,
    order: &Order,
) -> Result<(), MostroError> {
    // Get seller pubkey
    let seller_pubkey = order
        .get_seller_pubkey()
        .map_err(|_| MostroCantDo(CantDoReason::InvalidPubkey))?
        .to_string();
    // Get sender pubkey (trade key that authored the rumor)
    let sender_pubkey = event.sender.to_string();
    // Check if the pubkey is right
    if !is_admin && sender_pubkey != seller_pubkey {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // Settling the hold invoice
    if let Some(preimage) = order.preimage.as_ref() {
        escrow.settle_hold_invoice(preimage).await?;
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

/// Enqueue an order-type message onto the restore-session queue.
///
/// The scheduler drains `queue_order_msg` before `queue_restore_session_msg`
/// in each tick, so an AddInvoice enqueued via `enqueue_order_msg` would be
/// sent BEFORE the restore-session response even when enqueued after it.
/// Routing the AddInvoice through the restore-session queue instead preserves
/// FIFO ordering relative to the restore response: the client receives the
/// restore-session list first, then the AddInvoice prompt for an order it now
/// knows about.
pub async fn enqueue_order_msg_on_restore_queue(
    order_id: Option<Uuid>,
    action: Action,
    payload: Option<Payload>,
    destination_key: PublicKey,
    trade_index: Option<i64>,
) {
    let message = Message::new_order(order_id, None, trade_index, action, payload);
    MESSAGE_QUEUES
        .queue_restore_session_msg
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
    let order_id = order_msg.id.ok_or(MostroCantDo(CantDoReason::NotFound))?;
    let order = Order::by_id(pool, order_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    if let Some(order) = order {
        Ok(order)
    } else {
        Err(MostroCantDo(CantDoReason::NotFound))
    }
}

/// Efficiently retrieves multiple orders by their IDs for a specific user
///
/// # Arguments
/// * `pool` - Database connection pool
/// * `orders` - Vector of order IDs as UUIDs
/// * `user_pubkey` - Public key of the user requesting the orders
///
/// # Returns
/// * `Result<Vec<Order>, MostroError>` - Vector of found orders that belong to the user, empty if no orders found or input is empty
///
/// # Behavior
/// - Returns empty vector if input `orders` is empty
/// - Returns only the orders that exist in the database AND belong to the user (as buyer or seller)
/// - Uses a single SQL query with IN clause and user validation for efficiency
/// - Validates that the user has access to the requested orders
pub async fn get_user_orders_by_id(
    pool: &Pool<Sqlite>,
    orders: &[Uuid],
    user_pubkey: &str,
) -> Result<Vec<Order>, MostroError> {
    // Return empty vector if no orders requested
    if orders.is_empty() {
        return Ok(Vec::new());
    }

    let mut query_builder = QueryBuilder::new("SELECT * FROM orders WHERE id IN (");

    {
        let mut separated = query_builder.separated(", ");
        for order_id in orders {
            separated.push_bind(order_id);
        }
    }

    query_builder.push(") AND (");
    query_builder.push("master_buyer_pubkey = ");
    query_builder.push_bind(user_pubkey);
    query_builder.push(" OR master_seller_pubkey = ");
    query_builder.push_bind(user_pubkey);
    query_builder.push(")");

    // Preserve the caller requested order sequence so that response payload matches
    query_builder.push(" ORDER BY CASE id");
    for (index, order_id) in orders.iter().enumerate() {
        query_builder.push(" WHEN ");
        query_builder.push_bind(order_id);
        query_builder.push(" THEN ");
        query_builder.push_bind(index as i64);
    }
    query_builder.push(" END");

    let found_orders = query_builder
        .build_query_as::<Order>()
        .fetch_all(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(found_orders)
}

pub async fn validate_invoice(msg: &Message, order: &Order) -> Result<Option<String>, MostroError> {
    // init payment request to None
    let mut payment_request = None;
    // if payment request is present
    if let Some(pr) = msg.get_inner_message_kind().get_payment_request() {
        // Calculate total buyer fees (only Mostro fee)
        // Dev fee is NOT charged to buyer - it's paid by mostrod from its earnings
        let total_buyer_fees = order.fee;

        // if invoice is valid
        if is_valid_invoice(
            pr.clone(),
            Some(order.amount as u64),
            Some(total_buyer_fees as u64),
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

    let master_key = match user {
        Some(user) => user.to_string(),
        None => return Err(MostroCantDo(CantDoReason::InvalidPubkey)),
    };

    let reputation_data = match is_user_present(pool, master_key).await {
        Ok(user) => {
            let now = Timestamp::now().as_secs();
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
    use crate::bitcoin_price::BitcoinPriceManager;
    use mostro_core::message::{Message, MessageKind};
    use mostro_core::order::Order;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;
    use std::sync::Once;
    use uuid::{uuid, Uuid};
    // Setup function to initialize common settings or data before tests
    static INIT: Once = Once::new();

    fn initialize() {
        INIT.call_once(|| {
            // Any initialization code goes here
        });
    }

    #[test]
    // DEPRECATED(v0.19.0, #786): delete along with `stamp_protocol_version`.
    #[allow(deprecated)]
    fn stamp_protocol_version_follows_transport() {
        use mostro_core::message::Action;

        // A v2-stamped message (the `MessageKind::new` default) must be
        // downgraded to v1 when served over the gift-wrap transport...
        let mut msg = Message::new_order(
            Some(uuid!("308e1272-d5f4-47e6-bd97-3504baea9c23")),
            Some(1),
            None,
            Action::NewOrder,
            None,
        );
        stamp_protocol_version(&mut msg, Transport::GiftWrap);
        assert_eq!(msg.get_inner_message_kind().version, 1);

        // ...and stamped back to v2 over the nip44 direct transport.
        stamp_protocol_version(&mut msg, Transport::Nip44Direct);
        assert_eq!(msg.get_inner_message_kind().version, 2);
    }

    #[test]
    // DEPRECATED(v0.19.0, #786): delete along with `stamp_protocol_version`.
    #[allow(deprecated)]
    fn stamp_protocol_version_covers_all_variants() {
        use mostro_core::message::Action;

        let kind = MessageKind::new(None, Some(1), None, Action::CantDo, None);
        for mut msg in [
            Message::Order(kind.clone()),
            Message::Dispute(kind.clone()),
            Message::CantDo(kind.clone()),
            Message::Rate(kind.clone()),
            Message::Dm(kind.clone()),
            Message::Restore(kind.clone()),
        ] {
            stamp_protocol_version(&mut msg, Transport::GiftWrap);
            assert_eq!(msg.get_inner_message_kind().version, 1);
        }
    }

    async fn setup_orders_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();

        sqlx::query(include_str!("../migrations/20221222153301_orders.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../migrations/20251126120000_dev_fee.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!(
            "../migrations/20260530120000_cashu_escrow_fields.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    async fn insert_order(
        pool: &SqlitePool,
        id: Uuid,
        identity_buyer_pubkey: Option<&str>,
        identity_seller_pubkey: Option<&str>,
        creator_pubkey: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO orders (
                id,
                kind,
                event_id,
                creator_pubkey,
                status,
                premium,
                payment_method,
                amount,
                fiat_code,
                fiat_amount,
                created_at,
                expires_at,
                master_buyer_pubkey,
                master_seller_pubkey
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        )
        .bind(id)
        .bind("buy")
        .bind(id.simple().to_string())
        .bind(creator_pubkey)
        .bind("active")
        .bind(0_i64)
        .bind("ln")
        .bind(1_000_i64)
        .bind("USD")
        .bind(1_000_i64)
        .bind(1_000_i64)
        .bind(2_000_i64)
        .bind(identity_buyer_pubkey)
        .bind(identity_seller_pubkey)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn test_bytes_to_string() {
        initialize();
        let bytes = vec![0xde, 0xad, 0xbe, 0xef];
        let result = bytes_to_string(&bytes);
        assert_eq!(result, "deadbeef");
    }

    /// `NOSTR_CLIENT` is a process-wide `OnceLock` that `initialize()` and
    /// every other test's `init_globals()` also install into, so the two
    /// halves of this used to live in separate tests: one asserted
    /// `get_nostr_client()` errors while the lock reads `None`, the other
    /// asserted it succeeds once set.
    ///
    /// That split was racy. Reading `NOSTR_CLIENT.get()` and then asserting
    /// on `get_nostr_client()` is a check-then-act on shared state: a
    /// concurrent test can install the client in between and flip the result
    /// from `Err` to `Ok`. Serializing just these two wouldn't fix it either,
    /// since any `init_globals()` caller sets the same lock.
    ///
    /// A `OnceLock` is monotonic — once installed it never reverts — so the
    /// post-set direction is the only one that holds no matter who wins the
    /// race. Assert that, and only that. The uninitialized branch is not
    /// deterministically reachable in a shared-process test binary.
    #[tokio::test]
    async fn get_nostr_client_succeeds_once_the_global_is_installed() {
        initialize();
        // Idempotent: whoever won the race already installed an equivalent
        // client, and `set` on an initialized `OnceLock` is a no-op.
        let _ = NOSTR_CLIENT.set(Client::default());

        assert!(
            NOSTR_CLIENT.get().is_some(),
            "the global must be installed after set()"
        );
        assert!(
            get_nostr_client().is_ok(),
            "an installed global must be readable"
        );
        // Monotonic: a second read cannot regress to Err.
        assert!(get_nostr_client().is_ok(), "OnceLock must not revert");
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

    #[tokio::test]
    async fn test_get_user_orders_by_id_filters_and_preserves_order() {
        initialize();
        let pool = setup_orders_pool().await;
        let user_pubkey = "a".repeat(64);
        let other_pubkey = "b".repeat(64);

        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let third_id = Uuid::new_v4();

        insert_order(
            &pool,
            first_id,
            Some(&user_pubkey),
            Some(&other_pubkey),
            &user_pubkey,
        )
        .await;
        insert_order(
            &pool,
            second_id,
            Some(&other_pubkey),
            Some(&user_pubkey),
            &user_pubkey,
        )
        .await;
        insert_order(
            &pool,
            third_id,
            Some(&other_pubkey),
            Some(&other_pubkey),
            &other_pubkey,
        )
        .await;

        let requested = vec![second_id, first_id, third_id];

        let orders = get_user_orders_by_id(&pool, &requested, &user_pubkey)
            .await
            .unwrap();

        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].id, second_id);
        assert_eq!(orders[1].id, first_id);
    }

    #[tokio::test]
    async fn test_get_user_orders_by_id_empty_input() {
        initialize();
        let pool = setup_orders_pool().await;
        let user_pubkey = "a".repeat(64);

        let orders = get_user_orders_by_id(&pool, &[], &user_pubkey)
            .await
            .unwrap();

        assert!(orders.is_empty());
    }

    #[tokio::test]
    async fn test_get_order_returns_not_found_when_id_missing() {
        initialize();
        let pool = setup_orders_pool().await;
        let message = Message::Order(MessageKind::new(
            None,
            None,
            None,
            Action::AdminSettle,
            None,
        ));

        let err = get_order(&message, &pool).await.unwrap_err();
        assert!(matches!(
            err,
            MostroError::MostroCantDo(CantDoReason::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_get_order_returns_not_found_when_order_absent() {
        initialize();
        let pool = setup_orders_pool().await;
        let missing_id = Uuid::new_v4();
        let message = Message::Order(MessageKind::new(
            Some(missing_id),
            None,
            None,
            Action::AdminSettle,
            None,
        ));

        let err = get_order(&message, &pool).await.unwrap_err();
        assert!(matches!(
            err,
            MostroError::MostroCantDo(CantDoReason::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_get_order_returns_order_when_found() {
        initialize();
        let pool = setup_orders_pool().await;
        let user_pubkey = "a".repeat(64);
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id, Some(&user_pubkey), None, &user_pubkey).await;

        let message = Message::Order(MessageKind::new(
            Some(order_id),
            None,
            None,
            Action::AdminSettle,
            None,
        ));

        let order = get_order(&message, &pool).await.unwrap();
        assert_eq!(order.id, order_id);
    }

    #[test]
    fn test_get_dev_fee_basic() {
        // 1000 sats Mostro fee at 30% -> 300 sats
        let fee = calculate_dev_fee(1_000, 0.30);
        assert_eq!(fee, 300);
    }

    #[test]
    fn test_get_dev_fee_rounding() {
        // 333 * 0.30 = 99.9 -> rounds to 100
        let fee = calculate_dev_fee(333, 0.30);
        assert_eq!(fee, 100);
    }

    #[test]
    fn test_get_dev_fee_zero() {
        let fee = calculate_dev_fee(0, 0.30);
        assert_eq!(fee, 0);
    }

    #[test]
    fn test_get_dev_fee_tiny_amounts() {
        // With 30%, 1 * 0.30 = 0.3 -> 0
        let fee = calculate_dev_fee(1, 0.30);
        assert_eq!(fee, 0);
    }

    #[test]
    fn maker_bond_notional_uses_fixed_amount_directly() {
        // Phase 5: a fixed-price order carries its sats `amount`, so the
        // maker-bond notional is exactly that — no price lookup, no API
        // dependency in this path.
        let order = Order {
            amount: 50_000,
            fiat_code: "USD".to_string(),
            fiat_amount: 25,
            ..Default::default()
        };
        assert_eq!(maker_bond_notional_sats(&order).unwrap(), 50_000);
    }

    #[test]
    fn maker_bond_notional_range_sizes_against_max_at_price() {
        // Phase 6: a range order sizes the notional against `max_amount`
        // converted at the cached price: 100 fiat / 60_000 * 1e8 ≈ 166_666
        // sats. Unique fiat_code avoids clobbering the shared price cache.
        BitcoinPriceManager::set_price_for_test("T6RANGE", 60_000.0);
        let order = Order {
            amount: 0,
            min_amount: Some(10),
            max_amount: Some(100),
            fiat_code: "T6RANGE".to_string(),
            fiat_amount: 0,
            ..Default::default()
        };
        assert!(order.is_range_order());
        assert_eq!(maker_bond_notional_sats(&order).unwrap(), 166_666);
    }

    #[test]
    fn maker_bond_notional_range_rejects_non_positive_max() {
        // `is_range_order()` only checks `Some`-ness, so a `max_amount` of 0
        // still enters the range branch and must be rejected before bond
        // sizing (it would divide-by-zero in the proportional slash). A
        // `None` max can't reach here — `is_range_order()` would be false.
        let order = Order {
            amount: 0,
            min_amount: Some(10),
            max_amount: Some(0),
            fiat_code: "T6ZERO".to_string(),
            fiat_amount: 0,
            ..Default::default()
        };
        assert!(order.is_range_order());
        let err = maker_bond_notional_sats(&order).unwrap_err();
        assert!(
            matches!(err, MostroInternalErr(ServiceError::UnexpectedError(_))),
            "expected UnexpectedError for non-positive max_amount, got {err:?}"
        );
    }

    // ───────────────── shared helpers for the coverage tests below ─────────────────

    /// Install the process-wide globals handlers reach for. Idempotent —
    /// whichever test wins the OnceLock race, values stay consistent.
    fn init_globals() {
        let _ = MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
        let _ = NOSTR_CLIENT.set(Client::default());
    }

    /// In-memory pool with the full production schema.
    async fn migrated_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn insert_user_row(pool: &SqlitePool, pubkey: &str, last_trade_index: i64) {
        sqlx::query(
            "INSERT INTO users (pubkey, last_trade_index, created_at, total_rating, total_reviews) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(pubkey)
        .bind(last_trade_index)
        .bind(Timestamp::now().as_secs() as i64 - 86_400)
        .bind(4.5_f64)
        .bind(3_i64)
        .execute(pool)
        .await
        .unwrap();
    }

    fn base_order(kind: OrderKind, status: Status) -> Order {
        Order {
            id: Uuid::new_v4(),
            kind: kind.to_string(),
            status: status.to_string(),
            payment_method: "SEPA".to_string(),
            amount: 1_000,
            fee: 10,
            fiat_code: "USD".to_string(),
            fiat_amount: 100,
            created_at: Timestamp::now().as_secs() as i64,
            expires_at: Timestamp::now().as_secs() as i64 + 3_600,
            ..Default::default()
        }
    }

    /// Ensure the process-wide DB pool exists (migrated, in-memory).
    ///
    /// `DB_POOL` is a set-once global shared by every test in the binary, so
    /// whichever test runs first wins the race and installs its pool. That
    /// winner may have set up a different (or partial) schema, so we run our
    /// migrations against the live pool unconditionally — they are idempotent
    /// (tracked in `_sqlx_migrations`) and guarantee the tables these tests
    /// need exist regardless of who won.
    async fn ensure_global_db_pool() -> std::sync::Arc<SqlitePool> {
        if DB_POOL.get().is_none() {
            let pool = migrated_pool().await;
            let _ = DB_POOL.set(std::sync::Arc::new(pool));
        }
        let pool = get_db_pool();
        // Surface a migration failure instead of discarding it. Swallowing it
        // would hand back a pool with a partial schema, and the tests built on
        // it would fail much later with a confusing "no such table" instead of
        // the real cause. The migrator is idempotent (tracked in
        // `_sqlx_migrations`) and every `CREATE TABLE` in `migrations/` is
        // `IF NOT EXISTS`, so re-running it against an already-migrated pool
        // is a no-op — a failure here means a genuinely broken global pool.
        sqlx::migrate!("./migrations")
            .run(pool.as_ref())
            .await
            .expect("the process-global test pool must be fully migrated");
        pool
    }

    /// Freshly-signed BOLT11 invoice built locally (no network, no LND).
    fn build_test_invoice(amount_msat: u64, expiry_secs: u64) -> String {
        use bitcoin::hashes::{sha256, Hash};
        use bitcoin::secp256k1::{Secp256k1, SecretKey};
        use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
        use std::time::Duration;

        let secp = Secp256k1::new();
        let private_key = SecretKey::from_slice(&[0x42; 32]).expect("valid secret key");
        let payment_hash = sha256::Hash::hash(&[0u8; 32]);
        InvoiceBuilder::new(Currency::Bitcoin)
            .description("mostro util coverage invoice".into())
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([42u8; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .expiry_time(Duration::from_secs(expiry_secs))
            .amount_milli_satoshis(amount_msat)
            .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &private_key))
            .expect("valid signed invoice")
            .to_string()
    }

    // ───────────────────────── market quote & fees ─────────────────────────

    #[test]
    fn market_quote_converts_fiat_and_applies_premium() {
        BitcoinPriceManager::set_price_for_test("UTILQ1", 50_000.0);
        // 100 / 50_000 * 1e8 = 200_000 sats without premium…
        assert_eq!(get_market_quote(&100, "UTILQ1", 0).unwrap(), 200_000);
        // …and 10% premium knocks 10% off the sats value.
        assert_eq!(get_market_quote(&100, "UTILQ1", 10).unwrap(), 180_000);
    }

    #[test]
    fn market_quote_rejects_non_positive_price() {
        BitcoinPriceManager::set_price_for_test("UTILQ2", 0.0);
        let err = get_market_quote(&100, "UTILQ2", 0).unwrap_err();
        assert!(matches!(
            err,
            MostroError::MostroInternalErr(ServiceError::NoAPIResponse)
        ));
    }

    #[test]
    fn market_amount_and_fee_returns_quote_and_fee() {
        init_globals();
        BitcoinPriceManager::set_price_for_test("UTILQ3", 50_000.0);
        let (sats, fee) = get_market_amount_and_fee(100, "UTILQ3", 0).unwrap();
        assert_eq!(sats, 200_000);
        // Both candidate global configs carry fee = 0.
        assert_eq!(fee, 0);
        assert_eq!(get_fee(1_000), 0);
    }

    #[test]
    fn dev_fee_uses_configured_percentage() {
        init_globals();
        // Both candidate global configs carry dev_fee_percentage = 0.30.
        assert_eq!(get_dev_fee(1_000), 300);
    }

    #[test]
    fn maker_bond_notional_market_path_converts_at_cached_price() {
        BitcoinPriceManager::set_price_for_test("UTILQ4", 40_000.0);
        let order = Order {
            amount: 0,
            fiat_code: "UTILQ4".to_string(),
            fiat_amount: 100,
            ..Default::default()
        };
        // 100 / 40_000 * 1e8 = 250_000 sats.
        assert_eq!(maker_bond_notional_sats(&order).unwrap(), 250_000);
    }

    #[test]
    fn maker_bond_notional_rejects_non_positive_price() {
        BitcoinPriceManager::set_price_for_test("UTILQ5", 0.0);
        // Market-priced single order.
        let order = Order {
            amount: 0,
            fiat_code: "UTILQ5".to_string(),
            fiat_amount: 100,
            ..Default::default()
        };
        assert!(matches!(
            maker_bond_notional_sats(&order).unwrap_err(),
            MostroInternalErr(ServiceError::NoAPIResponse)
        ));
        // Range order against a non-positive price.
        let range = Order {
            amount: 0,
            min_amount: Some(10),
            max_amount: Some(100),
            fiat_code: "UTILQ5".to_string(),
            ..Default::default()
        };
        assert!(matches!(
            maker_bond_notional_sats(&range).unwrap_err(),
            MostroInternalErr(ServiceError::NoAPIResponse)
        ));
    }

    // ───────────────────────── expiration helpers ─────────────────────────

    #[test]
    fn expiration_date_defaults_and_clamps() {
        init_globals();
        let now = Timestamp::now().as_secs() as i64;
        // Both candidate global configs: 24h default, 15d max.
        let default_exp = get_expiration_date(None);
        assert!((default_exp - now - 86_400).abs() <= 2);

        // A user-supplied value inside the window is kept…
        let wanted = now + 100;
        assert_eq!(get_expiration_date(Some(wanted)), wanted);

        // …and one beyond the max is clamped to now + 15 days.
        let clamped = get_expiration_date(Some(now + 90 * 86_400));
        assert!((clamped - now - 15 * 86_400).abs() <= 2);
    }

    #[test]
    fn expiration_timestamp_by_kind_uses_config_and_ignores_unknown_kinds() {
        init_globals();
        let now = Timestamp::now().as_secs() as i64;
        // Known kinds resolve through the expiration configuration.
        let order_exp = get_expiration_timestamp_for_kind(NOSTR_ORDER_EVENT_KIND)
            .expect("order events always expire");
        assert!(order_exp > now);
        assert!(get_expiration_timestamp_for_kind(DM_EVENT_KIND).is_some());
        // Unknown kinds never get an expiration.
        assert!(get_expiration_timestamp_for_kind(12_345).is_none());
    }

    // ───────────────────────── order tags & publication ─────────────────────────

    #[tokio::test]
    async fn get_tags_for_new_order_covers_user_and_privacy_paths() {
        init_globals();
        let pool = migrated_pool().await;
        let keys = Keys::generate();
        let identity = Keys::generate().public_key();
        let trade = Keys::generate().public_key();
        let order = base_order(OrderKind::Sell, Status::Pending);

        // Known user → reputation-tagged event.
        insert_user_row(&pool, &identity.to_string(), 1).await;
        let tags = get_tags_for_new_order(&order, &pool, &identity, &trade, &keys)
            .await
            .unwrap();
        assert!(tags.is_some());

        // Unknown user in full-privacy shape (identity == trade) → zeroed reputation.
        let privacy = Keys::generate().public_key();
        let tags = get_tags_for_new_order(&order, &pool, &privacy, &privacy, &keys)
            .await
            .unwrap();
        assert!(tags.is_some());

        // Unknown user with mismatched keys → invalid pubkey.
        let stranger = Keys::generate().public_key();
        let err = get_tags_for_new_order(&order, &pool, &stranger, &trade, &keys)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            MostroInternalErr(ServiceError::InvalidPubkey)
        ));
    }

    #[tokio::test]
    async fn publish_order_persists_row_and_fails_only_at_broadcast() {
        init_globals();
        let pool = migrated_pool().await;
        let keys = Keys::generate();
        let trade = Keys::generate().public_key();
        let new_order = SmallOrder {
            kind: Some(OrderKind::Sell),
            amount: 1_000,
            fiat_code: "USD".to_string(),
            fiat_amount: 100,
            payment_method: "SEPA".to_string(),
            ..Default::default()
        };

        // Full-privacy maker (identity == trade). The offline client cannot
        // broadcast, so the pipeline must fail at the very last step…
        let res = publish_order(
            &pool,
            &keys,
            &new_order,
            trade,
            trade,
            trade,
            Some(1),
            Some(1),
        )
        .await;
        assert!(matches!(
            res,
            Err(MostroInternalErr(ServiceError::NostrError(_)))
        ));

        // …with the order row already persisted as pending.
        let (status, event_id, seller): (String, String, Option<String>) =
            sqlx::query_as("SELECT status, event_id, seller_pubkey FROM orders LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "pending");
        assert!(!event_id.is_empty());
        assert_eq!(seller.as_deref(), Some(trade.to_string().as_str()));
    }

    #[tokio::test]
    async fn publish_order_buy_kind_market_priced_sets_buyer_fields() {
        init_globals();
        let pool = migrated_pool().await;
        let keys = Keys::generate();
        let identity = Keys::generate().public_key();
        let trade = Keys::generate().public_key();
        insert_user_row(&pool, &identity.to_string(), 1).await;
        let new_order = SmallOrder {
            kind: Some(OrderKind::Buy),
            amount: 0, // market priced → price_from_api = true
            fiat_code: "USD".to_string(),
            fiat_amount: 100,
            payment_method: "SEPA".to_string(),
            ..Default::default()
        };

        let res = publish_order(
            &pool,
            &keys,
            &new_order,
            trade,
            identity,
            trade,
            Some(1),
            Some(2),
        )
        .await;
        assert!(matches!(
            res,
            Err(MostroInternalErr(ServiceError::NostrError(_)))
        ));

        let (kind, buyer, price_from_api): (String, Option<String>, bool) =
            sqlx::query_as("SELECT kind, buyer_pubkey, price_from_api FROM orders LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kind, "buy");
        assert_eq!(buyer.as_deref(), Some(trade.to_string().as_str()));
        assert!(price_from_api);
    }

    #[tokio::test]
    async fn publish_order_without_kind_is_rejected() {
        init_globals();
        let pool = migrated_pool().await;
        let keys = Keys::generate();
        let trade = Keys::generate().public_key();
        let new_order = SmallOrder {
            kind: None,
            amount: 1_000,
            fiat_code: "USD".to_string(),
            fiat_amount: 100,
            ..Default::default()
        };

        let res = publish_order(&pool, &keys, &new_order, trade, trade, trade, None, None).await;
        assert!(matches!(
            res,
            Err(MostroCantDo(CantDoReason::InvalidOrderKind))
        ));
    }

    #[tokio::test]
    async fn resume_publish_after_maker_bond_claims_and_publishes() {
        init_globals();
        let pool = migrated_pool().await;
        let keys = Keys::generate();
        let trade = Keys::generate().public_key();

        // Sell order parked at WaitingMakerBond, full-privacy maker.
        let mut order = base_order(OrderKind::Sell, Status::WaitingMakerBond);
        order.seller_pubkey = Some(trade.to_string());
        order.master_seller_pubkey = Some(trade.to_string());
        order.trade_index_seller = Some(1);
        let order = order.create(&pool).await.unwrap();

        let res = resume_publish_after_maker_bond(&pool, &keys, order, Some(1)).await;
        // Offline broadcast failure — but the CAS must have flipped the row.
        assert!(matches!(
            res,
            Err(MostroInternalErr(ServiceError::NostrError(_)))
        ));
        let (status,): (String,) = sqlx::query_as("SELECT status FROM orders LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[tokio::test]
    async fn resume_publish_after_maker_bond_buy_kind() {
        init_globals();
        let pool = migrated_pool().await;
        let keys = Keys::generate();
        let trade = Keys::generate().public_key();

        let mut order = base_order(OrderKind::Buy, Status::WaitingMakerBond);
        order.buyer_pubkey = Some(trade.to_string());
        order.master_buyer_pubkey = Some(trade.to_string());
        order.trade_index_buyer = Some(1);
        let order = order.create(&pool).await.unwrap();

        let res = resume_publish_after_maker_bond(&pool, &keys, order, None).await;
        assert!(matches!(
            res,
            Err(MostroInternalErr(ServiceError::NostrError(_)))
        ));
    }

    #[tokio::test]
    async fn resume_publish_after_maker_bond_skips_when_status_moved_on() {
        init_globals();
        let pool = migrated_pool().await;
        let keys = Keys::generate();

        // Already pending → CAS affects 0 rows → clean skip.
        let order = base_order(OrderKind::Sell, Status::Pending)
            .create(&pool)
            .await
            .unwrap();
        let res = resume_publish_after_maker_bond(&pool, &keys, order, None).await;
        assert!(res.is_ok(), "stale deferred publish must skip: {res:?}");
    }

    // ───────────────────── order event updates & ratings ─────────────────────

    #[tokio::test]
    async fn update_order_event_covers_reputation_paths() {
        init_globals();
        let gpool = ensure_global_db_pool().await;
        let keys = Keys::generate();
        let trade = Keys::generate().public_key();

        // Full privacy (master == trade, no user row) → zeroed reputation.
        let mut order = base_order(OrderKind::Sell, Status::Pending);
        order.seller_pubkey = Some(trade.to_string());
        order.master_seller_pubkey = Some(trade.to_string());
        let updated = update_order_event(&keys, Status::Pending, &order)
            .await
            .unwrap();
        assert_eq!(updated.status, Status::Pending.to_string());
        assert!(!updated.event_id.is_empty(), "a NIP-33 event must be built");

        // Known user → reputation from the users table.
        let master = Keys::generate().public_key();
        insert_user_row(&gpool, &master.to_string(), 1).await;
        let mut order2 = base_order(OrderKind::Sell, Status::Pending);
        order2.seller_pubkey = Some(trade.to_string());
        order2.master_seller_pubkey = Some(master.to_string());
        let updated2 = update_order_event(&keys, Status::Pending, &order2)
            .await
            .unwrap();
        assert!(!updated2.event_id.is_empty());

        // Buy order resolves the buyer-side keys.
        let mut order3 = base_order(OrderKind::Buy, Status::Pending);
        order3.buyer_pubkey = Some(trade.to_string());
        order3.master_buyer_pubkey = Some(trade.to_string());
        assert!(update_order_event(&keys, Status::Pending, &order3)
            .await
            .is_ok());

        // Unknown master ≠ trade → invalid pubkey.
        let stranger = Keys::generate().public_key();
        let mut order4 = base_order(OrderKind::Sell, Status::Pending);
        order4.seller_pubkey = Some(trade.to_string());
        order4.master_seller_pubkey = Some(stranger.to_string());
        let err = update_order_event(&keys, Status::Pending, &order4)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            MostroInternalErr(ServiceError::InvalidPubkey)
        ));

        // Non-pending status → no reputation lookup, no event emitted.
        let order5 = base_order(OrderKind::Sell, Status::Active);
        let updated5 = update_order_event(&keys, Status::Active, &order5)
            .await
            .unwrap();
        assert!(updated5.event_id.is_empty());
    }

    // ───────────────────────── nostr client plumbing ─────────────────────────

    #[tokio::test]
    async fn connect_nostr_builds_client_with_configured_relays() {
        init_globals();
        let client = connect_nostr().await.expect("client must build offline");
        assert!(
            !client.relays().await.is_empty(),
            "configured relays must be registered"
        );
    }

    #[tokio::test]
    async fn get_nostr_relays_returns_map_when_client_installed() {
        init_globals();
        assert!(get_nostr_relays().await.is_some());
    }

    #[tokio::test]
    async fn get_keys_follows_global_config() {
        init_globals();
        match get_keys() {
            Ok(keys) => assert_eq!(keys.public_key().to_hex().len(), 64),
            // The settings template carries a placeholder nsec — whichever
            // config won the global OnceLock race, behaviour must be typed.
            Err(MostroInternalErr(ServiceError::NostrError(_))) => {}
            Err(e) => panic!("unexpected error kind: {e:?}"),
        }
    }

    #[tokio::test]
    async fn publish_dev_fee_audit_event_fails_offline() {
        init_globals();
        let order = base_order(OrderKind::Sell, Status::Success);
        // Either the placeholder nsec fails key parsing (template config) or
        // the offline client fails the broadcast — both are typed errors.
        assert!(publish_dev_fee_audit_event(&order, "deadbeef")
            .await
            .is_err());
    }

    // ───────────────────────── LND-dependent early failures ─────────────────────────

    #[tokio::test]
    async fn show_hold_invoice_fails_fast_without_lnd() {
        init_globals();
        let keys = Keys::generate();
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();
        let order = base_order(OrderKind::Sell, Status::WaitingPayment);

        let res = show_hold_invoice(&keys, None, &buyer, &seller, order, None).await;
        assert!(res.is_err(), "no LND reachable in unit tests");
    }

    #[tokio::test]
    async fn invoice_subscribe_fails_fast_without_lnd() {
        init_globals();
        assert!(invoice_subscribe(vec![0u8; 32], None).await.is_err());
    }

    // ───────────────────────── messaging helpers ─────────────────────────

    #[tokio::test]
    async fn set_waiting_invoice_status_notifies_both_parties() {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = base_order(OrderKind::Sell, Status::Active);
        order.seller_pubkey = Some(seller.to_string());

        let amount = set_waiting_invoice_status(&mut order, buyer, Some(1))
            .await
            .unwrap();
        assert_eq!(amount, 1_000);
    }

    #[tokio::test]
    async fn rate_counterpart_enqueues_rate_requests() {
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();
        let order = base_order(OrderKind::Sell, Status::Success);
        assert!(rate_counterpart(&buyer, &seller, &order, None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn enqueue_helpers_push_into_global_queues() {
        // The queues are process-wide and concurrently drained by the
        // scheduler flush-job tests, so asserting on queue contents here
        // would race. The contract under test is "enqueue completes and
        // never panics"; delivery is covered by the scheduler tests.
        let key = Keys::generate().public_key();
        enqueue_cant_do_msg(Some(1), None, CantDoReason::NotFound, key).await;
        enqueue_restore_session_msg(None, key).await;
        enqueue_order_msg(Some(1), None, Action::Rate, None, key, None).await;
    }

    // ───────────────────────── escrow settlement ─────────────────────────

    struct FakeEscrow {
        settle_ok: bool,
    }

    #[async_trait::async_trait]
    impl crate::escrow::EscrowBackend for FakeEscrow {
        async fn create_hold_invoice(
            &mut self,
            _description: &str,
            _amount: i64,
        ) -> Result<(String, Vec<u8>, Vec<u8>), MostroError> {
            Ok((String::new(), vec![], vec![]))
        }

        async fn settle_hold_invoice(&mut self, _preimage: &str) -> Result<(), MostroError> {
            if self.settle_ok {
                Ok(())
            } else {
                Err(MostroInternalErr(ServiceError::HoldInvoiceError(
                    "forced failure".to_string(),
                )))
            }
        }

        async fn cancel_hold_invoice(&mut self, _hash: &str) -> Result<(), MostroError> {
            Ok(())
        }
    }

    fn unwrapped_from(trade: &Keys) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::new_order(None, None, None, Action::Release, None),
            signature: None,
            sender: trade.public_key(),
            identity: trade.public_key(),
            created_at: Timestamp::now(),
        }
    }

    #[tokio::test]
    async fn settle_seller_hold_invoice_success_and_error_paths() {
        let seller = Keys::generate();
        let mut order = base_order(OrderKind::Sell, Status::Active);
        order.seller_pubkey = Some(seller.public_key().to_string());
        order.preimage = Some("00".repeat(32));

        // Seller with matching key and a preimage settles fine.
        let mut escrow = FakeEscrow { settle_ok: true };
        let event = unwrapped_from(&seller);
        assert!(
            settle_seller_hold_invoice(&event, &mut escrow, Action::Release, false, &order)
                .await
                .is_ok()
        );

        // A non-seller sender is rejected unless admin.
        let interloper = Keys::generate();
        let event = unwrapped_from(&interloper);
        let err = settle_seller_hold_invoice(&event, &mut escrow, Action::Release, false, &order)
            .await
            .unwrap_err();
        assert!(matches!(err, MostroCantDo(CantDoReason::InvalidPubkey)));

        // Admin without a stored preimage → invalid invoice.
        let mut no_preimage = order.clone();
        no_preimage.preimage = None;
        let event = unwrapped_from(&interloper);
        let err = settle_seller_hold_invoice(
            &event,
            &mut escrow,
            Action::AdminSettle,
            true,
            &no_preimage,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MostroCantDo(CantDoReason::InvalidInvoice)));

        // Escrow backend failures propagate.
        let mut failing = FakeEscrow { settle_ok: false };
        let event = unwrapped_from(&seller);
        assert!(
            settle_seller_hold_invoice(&event, &mut failing, Action::Release, false, &order)
                .await
                .is_err()
        );
    }

    // ───────────────────────── dispute & invoice lookups ─────────────────────────

    #[tokio::test]
    async fn get_dispute_resolves_or_rejects() {
        let pool = migrated_pool().await;

        // No id in the message.
        let msg = Message::Dispute(MessageKind::new(None, None, None, Action::Dispute, None));
        assert!(get_dispute(&msg, &pool).await.is_err());

        // Unknown id.
        let msg = Message::Dispute(MessageKind::new(
            Some(Uuid::new_v4()),
            None,
            None,
            Action::Dispute,
            None,
        ));
        assert!(get_dispute(&msg, &pool).await.is_err());

        // Known dispute row resolves.
        let dispute_id = Uuid::new_v4();
        let order_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO disputes (id, order_id, status, order_previous_status, created_at) \
             VALUES (?, ?, 'initiated', 'active', ?)",
        )
        .bind(dispute_id)
        .bind(order_id)
        .bind(Timestamp::now().as_secs() as i64)
        .execute(&pool)
        .await
        .unwrap();
        let msg = Message::Dispute(MessageKind::new(
            Some(dispute_id),
            None,
            None,
            Action::Dispute,
            None,
        ));
        let dispute = get_dispute(&msg, &pool).await.unwrap();
        assert_eq!(dispute.order_id, order_id);
    }

    #[tokio::test]
    async fn validate_invoice_paths() {
        init_globals();
        let mut order = base_order(OrderKind::Sell, Status::Active);
        order.amount = 1_100;
        order.fee = 100;

        // No payment request in the message → nothing to validate.
        let msg = Message::new_order(None, None, None, Action::AddInvoice, None);
        assert_eq!(validate_invoice(&msg, &order).await.unwrap(), None);

        // Garbage payment request → invalid invoice.
        let msg = Message::new_order(
            None,
            None,
            None,
            Action::AddInvoice,
            Some(Payload::PaymentRequest(
                None,
                "notaninvoice".to_string(),
                None,
            )),
        );
        let err = validate_invoice(&msg, &order).await.unwrap_err();
        assert!(matches!(err, MostroCantDo(CantDoReason::InvalidInvoice)));

        // Freshly-built invoice for amount - fee = 1000 sats validates.
        let pr = build_test_invoice(1_000_000, 86_400);
        let msg = Message::new_order(
            None,
            None,
            None,
            Action::AddInvoice,
            Some(Payload::PaymentRequest(None, pr.clone(), None)),
        );
        assert_eq!(validate_invoice(&msg, &order).await.unwrap(), Some(pr));
    }

    // ───────────────────────── taker reputation notification ─────────────────────────

    #[tokio::test]
    async fn notify_taker_reputation_covers_all_branches() {
        init_globals();
        let pool = migrated_pool().await;
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();
        let master = Keys::generate().public_key();

        // Sell order without master buyer key → invalid pubkey.
        let order = base_order(OrderKind::Sell, Status::WaitingBuyerInvoice);
        let err = notify_taker_reputation(&pool, &order).await.unwrap_err();
        assert!(matches!(err, MostroCantDo(CantDoReason::InvalidPubkey)));

        // Sell + WaitingBuyerInvoice → PayInvoice to seller (unknown taker →
        // zeroed reputation).
        let mut order = base_order(OrderKind::Sell, Status::WaitingBuyerInvoice);
        order.master_buyer_pubkey = Some(master.to_string());
        order.seller_pubkey = Some(seller.to_string());
        assert!(notify_taker_reputation(&pool, &order).await.is_ok());

        // Known taker → reputation loaded from the users table.
        insert_user_row(&pool, &master.to_string(), 1).await;
        assert!(notify_taker_reputation(&pool, &order).await.is_ok());

        // Buy + WaitingBuyerInvoice → maker is adding the invoice → no-op Ok.
        let mut order = base_order(OrderKind::Buy, Status::WaitingBuyerInvoice);
        order.master_seller_pubkey = Some(master.to_string());
        assert!(notify_taker_reputation(&pool, &order).await.is_ok());

        // Buy + WaitingPayment → AddInvoice to buyer.
        let mut order = base_order(OrderKind::Buy, Status::WaitingPayment);
        order.master_seller_pubkey = Some(master.to_string());
        order.buyer_pubkey = Some(buyer.to_string());
        assert!(notify_taker_reputation(&pool, &order).await.is_ok());

        // Sell + WaitingPayment → not allowed.
        let mut order = base_order(OrderKind::Sell, Status::WaitingPayment);
        order.master_buyer_pubkey = Some(master.to_string());
        let err = notify_taker_reputation(&pool, &order).await.unwrap_err();
        assert!(matches!(
            err,
            MostroCantDo(CantDoReason::NotAllowedByStatus)
        ));

        // Any other status → not allowed.
        let mut order = base_order(OrderKind::Sell, Status::Active);
        order.master_buyer_pubkey = Some(master.to_string());
        let err = notify_taker_reputation(&pool, &order).await.unwrap_err();
        assert!(matches!(
            err,
            MostroCantDo(CantDoReason::NotAllowedByStatus)
        ));
    }

    // ───────────────────────── fiat amount extraction ─────────────────────────

    #[test]
    fn fiat_amount_requested_covers_range_and_fixed_orders() {
        // Fixed order → always its own fiat amount.
        let order = base_order(OrderKind::Sell, Status::Pending);
        let msg = Message::new_order(None, None, None, Action::TakeSell, None);
        assert_eq!(get_fiat_amount_requested(&order, &msg), Some(100));

        // Range order with an out-of-bounds request → None.
        let mut range = base_order(OrderKind::Sell, Status::Pending);
        range.min_amount = Some(50);
        range.max_amount = Some(200);
        let msg = Message::new_order(
            None,
            None,
            None,
            Action::TakeSell,
            Some(Payload::Amount(1_000)),
        );
        assert_eq!(get_fiat_amount_requested(&range, &msg), None);

        // Range order without any requested amount → None.
        let msg = Message::new_order(None, None, None, Action::TakeSell, None);
        assert_eq!(get_fiat_amount_requested(&range, &msg), None);
    }
}
