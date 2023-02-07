use dotenvy::var;
use nostr_sdk::nostr::util::time::timestamp;
use nostr_sdk::prelude::*;
use sqlx::migrate::MigrateDatabase;
use sqlx::pool::Pool;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::models::{NewOrder, Order};

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
) -> anyhow::Result<Order> {
    let mut conn = pool.acquire().await?;
    let uuid = Uuid::new_v4();
    let mut buyer_pubkey = "";
    let mut seller_pubkey = "";
    let created_at = timestamp() as i64;
    if order.kind == crate::types::Kind::Buy {
        buyer_pubkey = initiator_pubkey;
    } else {
        seller_pubkey = initiator_pubkey;
    }
    let kind = order.kind.to_string();
    let status = order.status.to_string();

    let order = sqlx::query_as::<_, Order>(
        r#"
        INSERT INTO orders (
        id,
        kind,
        event_id,
        buyer_pubkey,
        seller_pubkey,
        status,
        prime,
        payment_method,
        amount,
        fiat_code,
        fiat_amount,
        created_at
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        RETURNING *
      "#,
    )
    .bind(uuid)
    .bind(kind)
    .bind(event_id)
    .bind(buyer_pubkey)
    .bind(seller_pubkey)
    .bind(status)
    .bind(order.prime)
    .bind(&order.payment_method)
    .bind(order.amount)
    .bind(&order.fiat_code)
    .bind(order.fiat_amount)
    .bind(created_at)
    .fetch_one(&mut conn)
    .await?;

    Ok(order)
}

pub async fn edit_order(
    pool: &SqlitePool,
    status: &crate::types::Status,
    order_id: Uuid,
    buyer_pubkey: &XOnlyPublicKey,
    buyer_invoice: &str,
    preimage: &str,
    hash: &str,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let status = status.to_string();
    let buyer_pubkey = buyer_pubkey.to_bech32()?;
    let rows_affected = sqlx::query!(
        r#"
    UPDATE orders
    SET
    buyer_pubkey = ?1,
    status = ?2,
    buyer_invoice = ?3,
    preimage = ?4,
    hash = ?5
    WHERE id = ?6
    "#,
        buyer_pubkey,
        status,
        buyer_invoice,
        preimage,
        hash,
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
    status: &crate::types::Status,
    event_id: &str,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let status = status.to_string();
    let rows_affected = sqlx::query!(
        r#"
            UPDATE orders
            SET
            status = ?1,
            event_id = ?2
            WHERE id = ?3
        "#,
        status,
        event_id,
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
