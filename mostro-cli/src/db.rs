use crate::util::get_mcli_path;
use anyhow::Result;
use mostro_core::order::SmallOrder;
use mostro_core::NOSTR_REPLACEABLE_EVENT_KIND;
use nip06::FromMnemonic;
use nostr_sdk::prelude::*;
use sqlx::pool::Pool;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use std::fs::File;
use std::path::Path;

pub async fn connect() -> Result<Pool<Sqlite>> {
    let mcli_dir = get_mcli_path();
    let mcli_db_path = format!("{}/mcli.db", mcli_dir);
    let db_url = format!("sqlite://{}", mcli_db_path);
    let pool: Pool<Sqlite>;
    if !Path::exists(Path::new(&mcli_db_path)) {
        if let Err(res) = File::create(&mcli_db_path) {
            println!("Error in creating db file: {}", res);
            return Err(res.into());
        }
        pool = SqlitePool::connect(&db_url).await?;
        println!("Creating database file with orders table...");
        sqlx::query(
            r#"
          CREATE TABLE IF NOT EXISTS orders (
              id TEXT PRIMARY KEY,
              kind TEXT NOT NULL,
              status TEXT NOT NULL,
              amount INTEGER NOT NULL,
              min_amount INTEGER,
              max_amount INTEGER,
              fiat_code TEXT NOT NULL,
              fiat_amount INTEGER NOT NULL,
              payment_method TEXT NOT NULL,
              premium INTEGER NOT NULL,
              trade_keys TEXT,
              counterparty_pubkey TEXT,
              is_mine BOOLEAN,
              buyer_invoice TEXT,
              buyer_token INTEGER,
              seller_token INTEGER,
              request_id INTEGER,
              created_at INTEGER,
              expires_at INTEGER
          );
          CREATE TABLE IF NOT EXISTS users (
              i0_pubkey char(64) PRIMARY KEY,
              mnemonic TEXT,
              last_trade_index INTEGER,
              created_at INTEGER
          );
          "#,
        )
        .execute(&pool)
        .await?;

        let mnemonic = match Mnemonic::generate(12) {
            Ok(m) => m.to_string(),
            Err(e) => {
                println!("Error generating mnemonic: {}", e);
                return Err(e.into());
            }
        };
        let user = User::new(mnemonic, &pool).await?;
        println!("User created with pubkey: {}", user.i0_pubkey);
    } else {
        pool = SqlitePool::connect(&db_url).await?;
    }

    Ok(pool)
}

#[derive(Debug, Default, Clone, sqlx::FromRow)]
pub struct User {
    /// The user's ID is the identity pubkey
    pub i0_pubkey: String,
    pub mnemonic: String,
    pub last_trade_index: Option<i64>,
    pub created_at: i64,
}

impl User {
    pub async fn new(mnemonic: String, pool: &SqlitePool) -> Result<Self> {
        let mut user = User::default();
        let account = NOSTR_REPLACEABLE_EVENT_KIND as u32;
        let i0_keys =
            Keys::from_mnemonic_advanced(&mnemonic, None, Some(account), Some(0), Some(0))?;
        user.i0_pubkey = i0_keys.public_key().to_string();
        user.created_at = chrono::Utc::now().timestamp();
        user.mnemonic = mnemonic;
        sqlx::query(
            r#"
                  INSERT INTO users (i0_pubkey, mnemonic, created_at)
                  VALUES (?, ?, ?)
                "#,
        )
        .bind(&user.i0_pubkey)
        .bind(&user.mnemonic)
        .bind(user.created_at)
        .execute(pool)
        .await?;

        Ok(user)
    }
    // Chainable setters
    pub fn set_mnemonic(&mut self, mnemonic: String) -> &mut Self {
        self.mnemonic = mnemonic;
        self
    }

    pub fn set_last_trade_index(&mut self, last_trade_index: i64) -> &mut Self {
        self.last_trade_index = Some(last_trade_index);
        self
    }

    // Applying changes to the database
    pub async fn save(&self, pool: &SqlitePool) -> Result<()> {
        sqlx::query(
            r#"
              UPDATE users 
              SET mnemonic = ?, last_trade_index = ?
              WHERE i0_pubkey = ?
              "#,
        )
        .bind(&self.mnemonic)
        .bind(self.last_trade_index)
        .bind(&self.i0_pubkey)
        .execute(pool)
        .await?;

        println!(
            "User with i0 pubkey {} updated in the database.",
            self.i0_pubkey
        );

        Ok(())
    }

    pub async fn get(pool: &SqlitePool) -> Result<User> {
        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT i0_pubkey, mnemonic, last_trade_index, created_at
            FROM users
            LIMIT 1
            "#,
        )
        .fetch_one(pool)
        .await?;

        Ok(user)
    }

    pub async fn get_next_trade_index(pool: SqlitePool) -> Result<i64> {
        let user = User::get(&pool).await?;
        match user.last_trade_index {
            Some(index) => Ok(index + 1),
            None => Ok(1),
        }
    }

    pub async fn get_identity_keys(pool: &SqlitePool) -> Result<Keys> {
        let user = User::get(pool).await?;
        let account = NOSTR_REPLACEABLE_EVENT_KIND as u32;
        let keys =
            Keys::from_mnemonic_advanced(&user.mnemonic, None, Some(account), Some(0), Some(0))?;

        Ok(keys)
    }

    pub async fn get_next_trade_keys(pool: &SqlitePool) -> Result<(Keys, i64)> {
        let mut trade_index = User::get_next_trade_index(pool.clone()).await?;
        trade_index -= 1;

        let user = User::get(pool).await?;
        let account = NOSTR_REPLACEABLE_EVENT_KIND as u32;
        match trade_index.try_into() {
            Ok(index) => {
                let keys = Keys::from_mnemonic_advanced(
                    &user.mnemonic,
                    None,
                    Some(account),
                    Some(0),
                    Some(index),
                )?;
                Ok((keys, trade_index))
            }
            Err(e) => {
                println!("Error: {}", e);
                Err(e.into())
            }
        }
    }
}

#[derive(Debug, Default, Clone, sqlx::FromRow)]
pub struct Order {
    pub id: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub amount: i64,
    pub fiat_code: String,
    pub min_amount: Option<i64>,
    pub max_amount: Option<i64>,
    pub fiat_amount: i64,
    pub payment_method: String,
    pub premium: i64,
    pub trade_keys: Option<String>,
    pub counterparty_pubkey: Option<String>,
    pub is_mine: Option<bool>,
    pub buyer_invoice: Option<String>,
    pub buyer_token: Option<u16>,
    pub seller_token: Option<u16>,
    pub request_id: Option<i64>,
    pub created_at: Option<i64>,
    pub expires_at: Option<i64>,
}

impl Order {
    pub async fn new(
        pool: &SqlitePool,
        order: SmallOrder,
        trade_keys: &Keys,
        request_id: Option<i64>,
    ) -> Result<Self> {
        let trade_keys_hex = trade_keys.secret_key().to_secret_hex();
        let id = match order.id {
            Some(id) => id.to_string(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        let order = Order {
            id: Some(id),
            kind: order.kind.as_ref().map(|k| k.to_string()),
            status: order.status.as_ref().map(|s| s.to_string()),
            amount: order.amount,
            fiat_code: order.fiat_code,
            min_amount: order.min_amount,
            max_amount: order.max_amount,
            fiat_amount: order.fiat_amount,
            payment_method: order.payment_method,
            premium: order.premium,
            trade_keys: Some(trade_keys_hex),
            counterparty_pubkey: None,
            is_mine: Some(true),
            buyer_invoice: None,
            buyer_token: None,
            seller_token: None,
            request_id,
            created_at: Some(chrono::Utc::now().timestamp()),
            expires_at: None,
        };

        sqlx::query(
            r#"
                  INSERT INTO orders (id, kind, status, amount, min_amount, max_amount,
                  fiat_code, fiat_amount, payment_method, premium, trade_keys,
                  counterparty_pubkey, is_mine, buyer_invoice, buyer_token, seller_token,
                  request_id, created_at, expires_at)
                  VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
        )
        .bind(&order.id)
        .bind(&order.kind)
        .bind(&order.status)
        .bind(order.amount)
        .bind(order.min_amount)
        .bind(order.max_amount)
        .bind(&order.fiat_code)
        .bind(order.fiat_amount)
        .bind(&order.payment_method)
        .bind(order.premium)
        .bind(&order.trade_keys)
        .bind(&order.counterparty_pubkey)
        .bind(order.is_mine)
        .bind(&order.buyer_invoice)
        .bind(order.buyer_token)
        .bind(order.seller_token)
        .bind(order.request_id)
        .bind(order.created_at)
        .bind(order.expires_at)
        .execute(pool)
        .await?;

        Ok(order)
    }

    // Setters encadenables
    pub fn set_kind(&mut self, kind: String) -> &mut Self {
        self.kind = Some(kind);
        self
    }

    pub fn set_status(&mut self, status: String) -> &mut Self {
        self.status = Some(status);
        self
    }

    pub fn set_amount(&mut self, amount: i64) -> &mut Self {
        self.amount = amount;
        self
    }

    pub fn set_fiat_code(&mut self, fiat_code: String) -> &mut Self {
        self.fiat_code = fiat_code;
        self
    }

    pub fn set_min_amount(&mut self, min_amount: i64) -> &mut Self {
        self.min_amount = Some(min_amount);
        self
    }

    pub fn set_max_amount(&mut self, max_amount: i64) -> &mut Self {
        self.max_amount = Some(max_amount);
        self
    }

    pub fn set_fiat_amount(&mut self, fiat_amount: i64) -> &mut Self {
        self.fiat_amount = fiat_amount;
        self
    }

    pub fn set_payment_method(&mut self, payment_method: String) -> &mut Self {
        self.payment_method = payment_method;
        self
    }

    pub fn set_premium(&mut self, premium: i64) -> &mut Self {
        self.premium = premium;
        self
    }

    pub fn set_counterparty_pubkey(&mut self, counterparty_pubkey: String) -> &mut Self {
        self.counterparty_pubkey = Some(counterparty_pubkey);
        self
    }

    pub fn set_trade_keys(&mut self, trade_keys: String) -> &mut Self {
        self.trade_keys = Some(trade_keys);
        self
    }

    pub fn set_is_mine(&mut self, is_mine: bool) -> &mut Self {
        self.is_mine = Some(is_mine);
        self
    }

    // Applying changes to the database
    pub async fn save(&self, pool: &SqlitePool) -> Result<()> {
        // Validation if an identity document is present
        if let Some(ref id) = self.id {
            sqlx::query(
                r#"
              UPDATE orders 
              SET kind = ?, status = ?, amount = ?, fiat_code = ?, min_amount = ?, max_amount = ?, 
                  fiat_amount = ?, payment_method = ?, premium = ?, trade_keys = ?, counterparty_pubkey = ?,
                  is_mine = ?, buyer_invoice = ?, created_at = ?, expires_at = ?, buyer_token = ?,
                seller_token = ?
              WHERE id = ?
              "#,
            )
            .bind(&self.kind)
            .bind(&self.status)
            .bind(self.amount)
            .bind(&self.fiat_code)
            .bind(self.min_amount)
            .bind(self.max_amount)
            .bind(self.fiat_amount)
            .bind(&self.payment_method)
            .bind(self.premium)
            .bind(&self.trade_keys)
            .bind(&self.counterparty_pubkey)
            .bind(self.is_mine)
            .bind(&self.buyer_invoice)
            .bind(self.created_at)
            .bind(self.expires_at)
            .bind(self.buyer_token)
            .bind(self.seller_token)
            .bind(id)
            .execute(pool)
            .await?;

            println!("Order with id {} updated in the database.", id);
        } else {
            return Err(anyhow::anyhow!("Order must have an ID to be updated."));
        }

        Ok(())
    }

    pub async fn save_new_id(
        pool: &SqlitePool,
        id: String,
        new_id: String,
    ) -> anyhow::Result<bool> {
        let rows_affected = sqlx::query(
            r#"
          UPDATE orders
          SET id = ?
          WHERE id = ?
        "#,
        )
        .bind(&new_id)
        .bind(&id)
        .execute(pool)
        .await?
        .rows_affected();

        Ok(rows_affected > 0)
    }

    pub async fn get_by_id(pool: &SqlitePool, id: &str) -> Result<Order> {
        let order = sqlx::query_as::<_, Order>(
            r#"
            SELECT * FROM orders WHERE id = ?
            LIMIT 1
            "#,
        )
        .bind(id)
        .fetch_one(pool)
        .await?;

        Ok(order)
    }

    pub async fn get_all(pool: &SqlitePool) -> Result<Vec<Order>> {
        let orders = sqlx::query_as::<_, Order>(r#"SELECT * FROM orders"#)
            .fetch_all(pool)
            .await?;
        Ok(orders)
    }
}
