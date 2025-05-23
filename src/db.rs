use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::pool::Pool;
use sqlx::sqlite::SqliteRow;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use std::fs::OpenOptions;
use std::path::Path;
use uuid::Uuid;

use crate::config::settings::Settings;

pub async fn connect() -> Result<Pool<Sqlite>, MostroError> {
    // Get mostro settings
    let db_settings = Settings::get_db();
    let mut db_url = db_settings.url.to_string();
    db_url.push_str("mostro.db");
    // Remove sqlite:// from db_url
    let tmp = db_url.replace("sqlite://", "");
    let db_path = Path::new(&tmp);
    let conn = if !db_path.exists() {
        let _file = OpenOptions::new()
            .write(true)
            .create_new(true) // fails if the file already exists
            .open(db_path)
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        match SqlitePool::connect(&db_url).await {
            Ok(pool) => {
                match sqlx::migrate!().run(&pool).await {
                    Ok(_) => {
                        tracing::info!(
                            "Successfully created Mostro database file at {}",
                            db_path.display(),
                        );
                    }
                    Err(e) => {
                        // Clean up the created file on migration failure
                        if let Err(cleanup_err) = std::fs::remove_file(db_path) {
                            tracing::error!(
                                error = %cleanup_err,
                                path = %db_path.display(),
                                "Failed to create database connection"
                            );
                        }
                        return Err(MostroInternalErr(ServiceError::DbAccessError(
                            e.to_string(),
                        )));
                    }
                }
                pool
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    path = %db_path.display(),
                    "Failed to create database connection"
                );
                return Err(MostroInternalErr(ServiceError::DbAccessError(
                    e.to_string(),
                )));
            }
        }
    } else {
        SqlitePool::connect(&db_url)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
    };
    Ok(conn)
}

pub async fn edit_buyer_pubkey_order(
    pool: &SqlitePool,
    order_id: Uuid,
    buyer_pubkey: Option<String>,
) -> Result<bool, MostroError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let result = sqlx::query!(
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

pub async fn edit_seller_pubkey_order(
    pool: &SqlitePool,
    order_id: Uuid,
    seller_pubkey: Option<String>,
) -> Result<bool, MostroError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let result = sqlx::query!(
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

pub async fn find_order_by_hash(pool: &SqlitePool, hash: &str) -> Result<Order, MostroError> {
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE hash = ?1
        "#,
    )
    .bind(hash)
    .fetch_one(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(order)
}

pub async fn find_order_by_date(pool: &SqlitePool) -> Result<Vec<Order>, MostroError> {
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(order)
}

pub async fn find_order_by_seconds(pool: &SqlitePool) -> Result<Vec<Order>, MostroError> {
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(order)
}

pub async fn find_dispute_by_order_id(
    pool: &SqlitePool,
    order_id: Uuid,
) -> Result<Dispute, MostroError> {
    let dispute = sqlx::query_as::<_, Dispute>(
        r#"
          SELECT *
          FROM disputes
          WHERE order_id == ?1
        "#,
    )
    .bind(order_id)
    .fetch_one(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(dispute)
}

pub async fn update_order_to_initial_state(
    pool: &SqlitePool,
    order_id: Uuid,
    amount: i64,
    fee: i64,
) -> Result<bool, MostroError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let status = Status::Pending.to_string();
    let hash: Option<String> = None;
    let preimage: Option<String> = None;
    let result = sqlx::query!(
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

pub async fn edit_master_buyer_pubkey_order(
    pool: &SqlitePool,
    order_id: Uuid,
    master_buyer_pubkey: Option<String>,
) -> Result<bool, MostroError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let result = sqlx::query!(
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

pub async fn edit_master_seller_pubkey_order(
    pool: &SqlitePool,
    order_id: Uuid,
    master_seller_pubkey: Option<String>,
) -> Result<bool, MostroError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let result = sqlx::query!(
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

pub async fn reset_order_taken_at_time(
    pool: &SqlitePool,
    order_id: Uuid,
) -> Result<bool, MostroError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let taken_at = 0;

    let result = sqlx::query!(
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

pub async fn update_order_invoice_held_at_time(
    pool: &SqlitePool,
    order_id: Uuid,
    invoice_held_at: i64,
) -> Result<bool, MostroError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let result = sqlx::query!(
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

pub async fn find_held_invoices(pool: &SqlitePool) -> Result<Vec<Order>, MostroError> {
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE invoice_held_at !=0 AND  status == 'active'
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(order)
}

pub async fn find_failed_payment(pool: &SqlitePool) -> Result<Vec<Order>, MostroError> {
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE failed_payment == true AND  status == 'settled-hold-invoice'
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(order)
}

pub async fn find_solver_pubkey(
    pool: &SqlitePool,
    solver_npub: String,
) -> Result<User, MostroError> {
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(user)
}

pub async fn is_user_present(pool: &SqlitePool, public_key: String) -> Result<User, MostroError> {
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(user)
}

pub async fn add_new_user(pool: &SqlitePool, new_user: User) -> Result<String, MostroError> {
    // Validate public key format (32-bytes hex)
    let created_at: Timestamp = Timestamp::now();
    let _result = sqlx::query(
        "
            INSERT INTO users (pubkey, is_admin, is_solver, is_banned, category, last_trade_index, total_reviews, total_rating, last_rating, max_rating, min_rating, created_at) 
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ",
    )
    .bind(new_user.pubkey.clone())
    .bind(new_user.is_admin)
    .bind(new_user.is_solver)
    .bind(new_user.is_banned)
    .bind(new_user.category)
    .bind(new_user.last_trade_index)
    .bind(new_user.total_reviews)
    .bind(new_user.total_rating)
    .bind(new_user.last_rating)
    .bind(new_user.max_rating)
    .bind(new_user.min_rating)
    .bind(created_at.to_string())
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(new_user.pubkey)
}

pub async fn update_user_trade_index(
    pool: &SqlitePool,
    public_key: String,
    trade_index: i64,
) -> Result<bool, MostroError> {
    // Validate public key format (32-bytes hex)
    if !public_key.chars().all(|c| c.is_ascii_hexdigit()) || public_key.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }
    // Validate trade_index
    if trade_index < 0 {
        return Err(MostroCantDo(CantDoReason::InvalidTradeIndex));
    }

    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    let result = sqlx::query!(
        r#"
            UPDATE users SET last_trade_index = ?1 WHERE pubkey = ?2
        "#,
        trade_index,
        public_key,
    )
    .execute(&mut conn)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

/// Check if the seller has a pending order in the database with status waiting-payment
pub async fn seller_has_pending_order(
    pool: &SqlitePool,
    pubkey: String,
) -> Result<bool, MostroError> {
    // Validate public key format (32-bytes hex)
    if !pubkey.chars().all(|c| c.is_ascii_hexdigit()) || pubkey.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = sqlx::query(
        r#"
            SELECT EXISTS (SELECT 1 FROM orders WHERE master_seller_pubkey = ?1 AND status = 'waiting-payment')
        "#, 
    )
    .bind(pubkey)
    .map(|row: SqliteRow| row.get(0))
    .fetch_one(&mut conn)
    .await.map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(rows_affected)
}

/// Check if the buyer has a pending order in the database with status waiting-buyer-invoice
pub async fn buyer_has_pending_order(
    pool: &SqlitePool,
    pubkey: String,
) -> Result<bool, MostroError> {
    // Validate public key format (32-bytes hex)
    if !pubkey.chars().all(|c| c.is_ascii_hexdigit()) || pubkey.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = sqlx::query(
        r#"
            SELECT EXISTS (SELECT 1 FROM orders WHERE master_buyer_pubkey = ?1 AND status = 'waiting-buyer-invoice')
        "#,
    )
    .bind(pubkey)
    .map(|row: SqliteRow| row.get(0))
    .fetch_one(&mut conn)
    .await.map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(rows_affected)
}

pub async fn update_user_rating(
    pool: &SqlitePool,
    public_key: String,
    last_rating: i64,
    min_rating: i64,
    max_rating: i64,
    total_reviews: i64,
    total_rating: f64,
) -> Result<bool, MostroError> {
    // Validate public key format (32-bytes hex)
    if !public_key.chars().all(|c| c.is_ascii_hexdigit()) || public_key.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }
    // Validate rating values
    if !(0..=5).contains(&last_rating) {
        return Err(MostroCantDo(CantDoReason::InvalidRating));
    }
    if !(0..=5).contains(&min_rating) || !(0..=5).contains(&max_rating) {
        return Err(MostroCantDo(CantDoReason::InvalidRating));
    }
    if MIN_RATING as i64 > last_rating || last_rating > MAX_RATING as i64 {
        return Err(MostroCantDo(CantDoReason::InvalidRating));
    }
    if total_reviews < 0 {
        return Err(MostroCantDo(CantDoReason::InvalidRating));
    }
    if total_rating < 0.0 || total_rating > (total_reviews * 5) as f64 {
        return Err(MostroCantDo(CantDoReason::InvalidRating));
    }
    if !(min_rating <= last_rating && last_rating <= max_rating) {
        return Err(MostroCantDo(CantDoReason::InvalidRating));
    }
    let result = sqlx::query!(
        r#"
            UPDATE users SET last_rating = ?1, min_rating = ?2, max_rating = ?3, total_reviews = ?4, total_rating = ?5 WHERE pubkey = ?6
        "#,
        last_rating,
        min_rating,
        max_rating,
        total_reviews,
        total_rating,
        public_key,
    )
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

pub async fn is_assigned_solver(
    pool: &SqlitePool,
    solver_pubkey: &str,
    order_id: Uuid,
) -> Result<bool, MostroError> {
    tracing::info!(
        "Solver_pubkey: {} assigned to order {}",
        solver_pubkey,
        order_id
    );
    let result = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM disputes WHERE solver_pubkey = ? AND order_id = ?)",
    )
    .bind(solver_pubkey)
    .bind(order_id)
    .map(|row: SqliteRow| row.get(0))
    .fetch_one(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(result)
}

pub async fn find_order_by_id(
    pool: &SqlitePool,
    order_id: Uuid,
    user_pubkey: &str,
) -> Result<Order, MostroError> {
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
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(order)
}
