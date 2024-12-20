use mostro_core::dispute::Dispute;
use mostro_core::order::Order;
use mostro_core::order::Status;
use mostro_core::user::User;
use nostr_sdk::prelude::*;
use sqlx::migrate::MigrateDatabase;
use sqlx::pool::Pool;
use sqlx::sqlite::SqliteRow;
use sqlx::Row;
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
    let expire_time = Timestamp::now();
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE expires_at < ?1 AND status == 'pending'
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
          WHERE taken_at < ?1 AND ( status == 'waiting-buyer-invoice' OR status == 'waiting-payment' )
        "#,
    )
    .bind(expire_time.to_string())
    .fetch_all(pool)
    .await?;

    Ok(order)
}

pub async fn find_dispute_by_order_id(
    pool: &SqlitePool,
    order_id: Uuid,
) -> anyhow::Result<Dispute> {
    let dispute = sqlx::query_as::<_, Dispute>(
        r#"
          SELECT *
          FROM disputes
          WHERE order_id == ?1
        "#,
    )
    .bind(order_id)
    .fetch_one(pool)
    .await?;

    Ok(dispute)
}

pub async fn update_order_to_initial_state(
    pool: &SqlitePool,
    order_id: Uuid,
    amount: i64,
    fee: i64,
) -> anyhow::Result<bool> {
    let mut conn = pool.acquire().await?;
    let status = Status::Pending.to_string();
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

pub async fn find_held_invoices(pool: &SqlitePool) -> anyhow::Result<Vec<Order>> {
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE invoice_held_at !=0 true AND  status == 'active'
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(order)
}

pub async fn find_failed_payment(pool: &SqlitePool) -> anyhow::Result<Vec<Order>> {
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE failed_payment == true AND  status == 'settled-hold-invoice'
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(order)
}

pub async fn find_solver_pubkey(pool: &SqlitePool, solver_npub: String) -> anyhow::Result<User> {
    let user = sqlx::query_as::<_, User>(
        r#"
          SELECT *
          FROM users
          WHERE pubkey == ?1 AND  is_solver == true
          LIMIT 1
        "#,
    )
    .bind(solver_npub)
    .fetch_one(pool)
    .await?;

    Ok(user)
}

pub async fn is_user_present(pool: &SqlitePool, public_key: String) -> anyhow::Result<User> {
    let user = sqlx::query_as::<_, User>(
        r#"
            SELECT *
            FROM users
            WHERE pubkey == ?1
            LIMIT 1
        "#,
    )
    .bind(public_key)
    .fetch_one(pool)
    .await?;

    Ok(user)
}

pub async fn is_assigned_solver(
    pool: &SqlitePool,
    solver_pubkey: &str,
    order_id: Uuid,
) -> anyhow::Result<bool> {
    println!("solver_pubkey: {}", solver_pubkey);
    println!("order_id: {}", order_id);
    let result = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM disputes WHERE solver_pubkey = ? AND order_id = ?)",
    )
    .bind(solver_pubkey)
    .bind(order_id)
    .map(|row: SqliteRow| row.get(0))
    .fetch_one(pool)
    .await?;

    Ok(result)
}

pub async fn find_order_by_id(
    pool: &SqlitePool,
    order_id: Uuid,
    user_pubkey: &str,
) -> anyhow::Result<Order> {
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE id = ?1 AND (buyer_pubkey = ?2 OR seller_pubkey = ?2)
        "#,
    )
    .bind(order_id)
    .bind(user_pubkey)
    .fetch_one(pool)
    .await?;

    Ok(order)
}
