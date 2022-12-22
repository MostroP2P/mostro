use sqlx::migrate::MigrateDatabase;
use sqlx::pool::Pool;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use std::env;

use crate::types::Order;

pub async fn connect() -> Result<Pool<Sqlite>, sqlx::Error> {
    let db_url = env::var("DATABASE_URL").expect("$DATABASE_URL is not set");
    if !Sqlite::database_exists(&db_url).await.unwrap_or(false) {
        panic!("Not database found, please create a new one first!");
    }
    let pool = SqlitePool::connect(&db_url).await?;

    Ok(pool)
}

pub async fn add_order(pool: &SqlitePool, order: &Order) -> anyhow::Result<i64> {
    let mut conn = pool.acquire().await?;
    let kind = order.kind.to_string();
    let status = order.status.to_string();
    let id = sqlx::query!(
        r#"
      INSERT INTO orders (
      kind,
      buyer_pubkey,
      seller_pubkey,
      status,
      description,
      payment_method,
      amount,
      fiat_code,
      fiat_amount
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
      "#,
        kind,
        "buyer pubkey",
        "seller pubkey",
        status,
        "description",
        order.payment_method,
        order.amount,
        order.fiat_code,
        order.fiat_amount
    )
    .execute(&mut conn)
    .await?
    .last_insert_rowid();

    Ok(id)
}
