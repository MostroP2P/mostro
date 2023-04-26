use dotenvy::var;
use nostr_sdk::prelude::*;
use sqlx::migrate::MigrateDatabase;
use sqlx::pool::Pool;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use uuid::Uuid;

use mostro_core::order::{NewOrder, Order};
use mostro_core::{Kind, Status};

pub async fn connect() -> Result<Pool<Sqlite>, sqlx::Error> {
    let db_url = var("DATABASE_URL").expect("DATABASE_URL is not set");
    if !Sqlite::database_exists(&db_url).await.unwrap_or(false) {
        panic!("Not database found, please create a new one first!");
    }
    let pool = SqlitePool::connect(&db_url).await?;

    Ok(pool)
}

pub async fn add_order(
    pool: &SqlitePool,
    order: &NewOrder,
    event_id: &str,
    initiator_pubkey: &str,
    master_pubkey: &str,
) -> anyhow::Result<Order> {
    let mut conn = pool.acquire().await?;
    let uuid = Uuid::new_v4();
    let mut buyer_pubkey: Option<String> = None;
    let mut master_buyer_pubkey: Option<String> = None;
    let mut seller_pubkey: Option<String> = None;
    let mut master_seller_pubkey: Option<String> = None;
    let created_at = Timestamp::now();
    if order.kind == Kind::Buy {
        buyer_pubkey = Some(initiator_pubkey.to_string());
        master_buyer_pubkey = Some(master_pubkey.to_string());
    } else {
        seller_pubkey = Some(initiator_pubkey.to_string());
        master_seller_pubkey = Some(master_pubkey.to_string());
    }
    let kind = order.kind.to_string();
    let status = order.status.to_string();
    let price_from_api = order.amount == 0;

    let order = sqlx::query_as::<_, Order>(
        r#"
        INSERT INTO orders (
        id,
        kind,
        event_id,
        creator_pubkey,
        buyer_pubkey,
        master_buyer_pubkey,
        seller_pubkey,
        master_seller_pubkey,
        status,
        premium,
        payment_method,
        amount,
        price_from_api,
        fiat_code,
        fiat_amount,
        buyer_invoice,
        created_at
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
        RETURNING *
      "#,
    )
    .bind(uuid)
    .bind(kind)
    .bind(event_id)
    .bind(initiator_pubkey)
    .bind(buyer_pubkey)
    .bind(master_buyer_pubkey)
    .bind(seller_pubkey)
    .bind(master_seller_pubkey)
    .bind(status)
    .bind(order.premium)
    .bind(&order.payment_method)
    .bind(order.amount)
    .bind(price_from_api)
    .bind(&order.fiat_code)
    .bind(order.fiat_amount)
    .bind(order.buyer_invoice.as_ref())
    .bind(created_at.as_i64())
    .fetch_one(&mut conn)
    .await?;

    Ok(order)
}

#[allow(clippy::too_many_arguments)]
pub async fn edit_order(
    pool: &SqlitePool,
    status: &Status,
    order_id: Uuid,
    buyer_pubkey: &XOnlyPublicKey,
    seller_pubkey: &XOnlyPublicKey,
    preimage: &str,
    hash: &str,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let status = status.to_string();
    let buyer_pubkey = buyer_pubkey.to_bech32()?;
    let seller_pubkey = seller_pubkey.to_bech32()?;
    let rows_affected = sqlx::query!(
        r#"
    UPDATE orders
    SET
    buyer_pubkey = ?1,
    seller_pubkey = ?2,
    status = ?3,
    preimage = ?4,
    hash = ?5
    WHERE id = ?6
    "#,
        buyer_pubkey,
        seller_pubkey,
        status,
        preimage,
        hash,
        order_id
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn edit_buyer_invoice_order(
    pool: &SqlitePool,
    order_id: Uuid,
    buyer_invoice: &str,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            buyer_invoice = ?1
            WHERE id = ?2
        "#,
        buyer_invoice,
        order_id
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn edit_buyer_pubkey_order(
    pool: &SqlitePool,
    order_id: Uuid,
    buyer_pubkey: Option<String>,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            buyer_pubkey = ?1
            WHERE id = ?2
        "#,
        buyer_pubkey,
        order_id
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn edit_seller_pubkey_order(
    pool: &SqlitePool,
    order_id: Uuid,
    seller_pubkey: Option<String>,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            seller_pubkey = ?1
            WHERE id = ?2
        "#,
        seller_pubkey,
        order_id
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn update_order_event_id_status(
    pool: &SqlitePool,
    order_id: Uuid,
    status: &Status,
    event_id: &str,
    amount: i64,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let status = status.to_string();
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            status = ?1,
            amount = ?2,
            event_id = ?3
            WHERE id = ?4
        "#,
        status,
        amount,
        event_id,
        order_id,
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn update_order_event_id_rate_status(
    pool: &SqlitePool,
    order_id: Uuid,
    buyer_sent_rate: bool,
    seller_sent_rate: bool,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            buyer_sent_rate = ?1,
            seller_sent_rate = ?2            
            WHERE id = ?3
        "#,
        buyer_sent_rate,
        seller_sent_rate,
        order_id,
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn find_order_by_event_id(pool: &SqlitePool, event_id: &str) -> anyhow::Result<Order> {
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE event_id = ?1
        "#,
    )
    .bind(event_id)
    .fetch_one(pool)
    .await?;

    Ok(order)
}

pub async fn find_order_by_hash(pool: &SqlitePool, hash: &str) -> anyhow::Result<Order> {
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE hash = ?1
        "#,
    )
    .bind(hash)
    .fetch_one(pool)
    .await?;

    Ok(order)
}

pub async fn find_order_by_date(pool: &SqlitePool) -> anyhow::Result<Vec<Order>> {
    let exp_hours = var("EXP_HOURS")
        .expect("EXP_HOURS is not set")
        .as_str()
        .parse::<u64>()
        .unwrap();
    let expire_time = Timestamp::now() - (3600 * exp_hours);
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE created_at < ?1 AND status == 'Pending'
        "#,
    )
    .bind(expire_time.to_string())
    .fetch_all(pool)
    .await?;

    Ok(order)
}

pub async fn update_order_to_initial_state(
    pool: &SqlitePool,
    order_id: Uuid,
    amount: i64,
    fee: i64,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let status = "Pending".to_string();
    let hash: Option<String> = None;
    let preimage: Option<String> = None;
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            status = ?1,
            amount = ?2,
            fee = ?3,
            hash = ?4,
            preimage = ?5,
            taken_at = ?6,
            invoice_held_at = ?7
            WHERE id = ?8
        "#,
        status,
        amount,
        fee,
        hash,
        preimage,
        0,
        0,
        order_id,
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn init_cancel_order(pool: &SqlitePool, order: &Order) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            cancel_initiator_pubkey = ?1,
            buyer_cooperativecancel = ?2,
            seller_cooperativecancel = ?3
            WHERE id = ?4
        "#,
        order.cancel_initiator_pubkey,
        order.buyer_cooperativecancel,
        order.seller_cooperativecancel,
        order.id,
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn edit_master_buyer_pubkey_order(
    pool: &SqlitePool,
    order_id: Uuid,
    master_buyer_pubkey: Option<String>,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            master_buyer_pubkey = ?1
            WHERE id = ?2
        "#,
        master_buyer_pubkey,
        order_id
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn edit_master_seller_pubkey_order(
    pool: &SqlitePool,
    order_id: Uuid,
    master_seller_pubkey: Option<String>,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            master_seller_pubkey = ?1
            WHERE id = ?2
        "#,
        master_seller_pubkey,
        order_id
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn find_order_by_id(pool: &SqlitePool, id: Uuid) -> anyhow::Result<Order> {
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE id = ?1
        "#,
    )
    .bind(id)
    .fetch_one(pool)
    .await?;

    Ok(order)
}
