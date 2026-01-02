use crate::config::settings::get_db_pool;
use crate::config::MOSTRO_DB_PASSWORD;
use crate::db::is_user_present;
use crate::lightning::invoice::is_valid_invoice;
use crate::nip33::{new_event, order_to_tags};
use crate::util::pricing::{get_expiration_date, get_fee};
use crate::util::queues::enqueue_order_msg;
use crate::NOSTR_CLIENT;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, QueryBuilder, Sqlite, SqlitePool};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::info;
use uuid::Uuid;

// Redefined for convenience
type OrderKind = mostro_core::order::Kind;

// Private helper function for get_ratings_for_pending_order
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

/// Checks whether an order qualifies as a full privacy order and returns corresponding event tags.
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

        if let Ok(client) = crate::util::nostr::get_nostr_client() {
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

/// Efficiently retrieves multiple orders by their IDs for a specific user
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

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::message::{Message, MessageKind};
    use mostro_core::order::Order as SmallOrderTest;
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;
    use uuid::{uuid, Uuid};

    async fn setup_orders_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();

        sqlx::query(include_str!("../../migrations/20221222153301_orders.sql"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(include_str!("../../migrations/20251126120000_dev_fee.sql"))
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

    #[tokio::test]
    async fn test_get_fiat_amount_requested() {
        let uuid = uuid!("308e1272-d5f4-47e6-bd97-3504baea9c23");
        let order = SmallOrderTest {
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
        let pool = setup_orders_pool().await;
        let user_pubkey = "a".repeat(64);

        let orders = get_user_orders_by_id(&pool, &[], &user_pubkey)
            .await
            .unwrap();

        assert!(orders.is_empty());
    }
}
