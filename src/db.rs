use mostro_core::order::{Kind, Order, SmallOrder, Status};
use nostr_sdk::prelude::*;
use sqlx::migrate::MigrateDatabase;
use sqlx::pool::Pool;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::cli::settings::Settings;

pub async fn connect() -> Result<Pool<Sqlite>, sqlx::Error> {
    let db_settings = Settings::get_db();
    let mut db_url = db_settings.url;
    db_url.push_str("mostro.db");
    if !Sqlite::database_exists(&db_url).await.unwrap_or(false) {
        panic!("Not database found, please create a new one first!");
    }
    let pool = SqlitePool::connect(&db_url).await?;

    Ok(pool)
}

pub async fn add_order(
    pool: &SqlitePool,
    order: &SmallOrder,
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
    let mut kind = "Sell".to_string();
    if order.kind == Some(Kind::Buy) {
        kind = "Buy".to_string();
        buyer_pubkey = Some(initiator_pubkey.to_string());
        master_buyer_pubkey = Some(master_pubkey.to_string());
    } else {
        seller_pubkey = Some(initiator_pubkey.to_string());
        master_seller_pubkey = Some(master_pubkey.to_string());
    }
    let status = if let Some(status) = order.status {
        status.to_string()
    } else {
        "Pending".to_string()
    };
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
    let mostro_settings = Settings::get_mostro();
    let status = status.to_string();
    // We calculate the bot fee
    let fee = mostro_settings.fee;
    let fee = fee * amount as f64;
    let fee = fee.round() as i64;

    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            status = ?1,
            amount = ?2,
            fee = ?3,
            event_id = ?4
            WHERE id = ?5
        "#,
        status,
        amount,
        fee,
        event_id,
        order_id,
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
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
    let mostro_settings = Settings::get_mostro();
    let exp_hours = mostro_settings.expiration_hours as u64;
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

pub async fn find_order_by_seconds(pool: &SqlitePool) -> anyhow::Result<Vec<Order>> {
    let mostro_settings = Settings::get_mostro();
    let exp_seconds = mostro_settings.expiration_seconds as u64;
    let expire_time = Timestamp::now() - exp_seconds;
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE taken_at < ?1 AND ( status == 'WaitingBuyerInvoice' OR status == 'WaitingPayment' )
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

pub async fn reset_order_taken_at_time(pool: &SqlitePool, order_id: Uuid) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let taken_at = 0;

    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            taken_at = ?1
            WHERE id = ?2
        "#,
        taken_at,
        order_id,
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}

pub async fn update_order_invoice_held_at_time(
    pool: &SqlitePool,
    order_id: Uuid,
    invoice_held_at: i64,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            invoice_held_at = ?1
            WHERE id = ?2
        "#,
        invoice_held_at,
        order_id,
    )
    .execute(&mut conn)
    .await?
    .rows_affected();

    Ok(rows_affected > 0)
}
