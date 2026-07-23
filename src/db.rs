use crate::config::settings::Settings;
use mostro_core::order::Kind as OrderKind;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::pool::Pool;
use sqlx::sqlite::SqliteRow;
use sqlx::{AssertSqlSafe, Row, Sqlite, SqlitePool};
use std::collections::HashSet;
#[cfg(unix)]
use std::fs::{set_permissions, Permissions};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

// Constants for status filtering used across restore session functions
const EXCLUDED_ORDER_STATUSES: &str = "'expired','success','canceled','dispute','canceledbyadmin','completedbyadmin','settledbyadmin','cooperativelycanceled'";
const ACTIVE_DISPUTE_STATUSES: &str = "'initiated','in-progress'";

/// Terminal order statuses for the Phase 2 active-trade-pubkey cache: an
/// order in any of these will never legitimately originate further trade-key
/// messages, so its participants drop out of the "known keys" set.
///
/// This is deliberately [`EXCLUDED_ORDER_STATUSES`] **minus `'dispute'`** — a
/// disputed order is still active (buyer, seller and the assigned solver keep
/// messaging), so its trade keys must stay fast-pathed. See
/// `find_active_trade_pubkeys` and docs/TRANSPORT_V2_SPEC.md §6 Phase 2.
const TERMINAL_ORDER_STATUSES: &str = "'expired','success','canceled','canceledbyadmin','completedbyadmin','settledbyadmin','cooperativelycanceled'";

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Collect the "known keys" the Phase 2 anti-spam gate fast-paths: the trade
/// pubkeys (buyer / seller / creator) of every **non-terminal** order, plus
/// the solver pubkey of every **active** dispute (spec §6 Phase 2).
///
/// Status lists are compile-time constants ([`TERMINAL_ORDER_STATUSES`],
/// [`ACTIVE_DISPUTE_STATUSES`]), never user input, so the inline
/// interpolation carries no injection risk — same pattern the restore-session
/// queries already use. The result is deduplicated.
pub async fn find_active_trade_pubkeys(pool: &SqlitePool) -> Result<Vec<String>, MostroError> {
    let mut keys: HashSet<String> = HashSet::new();

    // Order participants of every still-active order (disputed orders
    // included — `TERMINAL_ORDER_STATUSES` excludes `'dispute'`).
    let order_query = format!(
        "SELECT buyer_pubkey, seller_pubkey, creator_pubkey FROM orders WHERE status NOT IN ({TERMINAL_ORDER_STATUSES})"
    );
    let order_rows = sqlx::query(AssertSqlSafe(order_query))
        .fetch_all(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    for row in order_rows {
        for col in ["buyer_pubkey", "seller_pubkey", "creator_pubkey"] {
            if let Ok(Some(pk)) = row.try_get::<Option<String>, _>(col) {
                if !pk.is_empty() {
                    keys.insert(pk);
                }
            }
        }
    }

    // Assigned solvers of active disputes (so admin-settle/cancel/take from
    // the solver's key fast-paths instead of hitting the first-contact lane).
    let dispute_query = format!(
        "SELECT solver_pubkey FROM disputes WHERE status IN ({ACTIVE_DISPUTE_STATUSES}) AND solver_pubkey IS NOT NULL"
    );
    let dispute_rows = sqlx::query(AssertSqlSafe(dispute_query))
        .fetch_all(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    for row in dispute_rows {
        if let Ok(Some(pk)) = row.try_get::<Option<String>, _>("solver_pubkey") {
            if !pk.is_empty() {
                keys.insert(pk);
            }
        }
    }

    Ok(keys.into_iter().collect())
}

/// Helper function to rebuild disputes table without token columns when DROP COLUMN is unsupported.
async fn rebuild_disputes_table_without_tokens(pool: &SqlitePool) -> Result<(), MostroError> {
    tracing::info!("Rebuilding disputes table without token columns (SQLite compatibility mode)");

    // Create temporary table with new schema (without token columns)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS disputes_temp (
            id char(36) primary key not null,
            order_id char(36) unique not null,
            status varchar(10) not null,
            order_previous_status varchar(10) not null,
            solver_pubkey char(64),
            created_at integer not null,
            taken_at integer default 0
        )
        "#,
    )
    .execute(pool)
    .await
    .map_err(|e| {
        MostroInternalErr(ServiceError::DbAccessError(format!(
            "Failed to create temporary disputes table: {}",
            e
        )))
    })?;

    // Copy data from original table to temporary table (excluding token columns)
    sqlx::query(
        r#"
        INSERT INTO disputes_temp (id, order_id, status, order_previous_status, solver_pubkey, created_at, taken_at)
        SELECT id, order_id, status, order_previous_status, solver_pubkey, created_at, taken_at
        FROM disputes
        "#,
    )
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(format!(
        "Failed to copy data to temporary table: {}", e
    ))))?;

    // Drop original table
    sqlx::query("DROP TABLE disputes")
        .execute(pool)
        .await
        .map_err(|e| {
            MostroInternalErr(ServiceError::DbAccessError(format!(
                "Failed to drop original disputes table: {}",
                e
            )))
        })?;

    // Rename temporary table to disputes
    sqlx::query("ALTER TABLE disputes_temp RENAME TO disputes")
        .execute(pool)
        .await
        .map_err(|e| {
            MostroInternalErr(ServiceError::DbAccessError(format!(
                "Failed to rename temporary table: {}",
                e
            )))
        })?;

    tracing::info!("Successfully rebuilt disputes table without token columns");
    Ok(())
}

/// Migrates legacy disputes table by removing deprecated buyer_token and seller_token columns if present.
///
/// This function uses transactions for atomic operations and includes fallback logic for older SQLite versions
/// that don't support ALTER TABLE DROP COLUMN. The function handles both cases where columns exist (legacy databases)
/// and don't exist (newer databases).
async fn migrate_remove_token_columns(pool: &SqlitePool) -> Result<(), MostroError> {
    // Check if token columns exist
    let buyer_token_exists = sqlx::query_scalar::<_, i32>(
        r#"
        SELECT COUNT(*) 
        FROM pragma_table_info('disputes') 
        WHERE name = 'buyer_token'
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
        > 0;

    let seller_token_exists = sqlx::query_scalar::<_, i32>(
        r#"
        SELECT COUNT(*) 
        FROM pragma_table_info('disputes') 
        WHERE name = 'seller_token'
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
        > 0;

    // If no token columns exist, no migration needed
    if !buyer_token_exists && !seller_token_exists {
        tracing::debug!(
            "No deprecated token columns found in disputes table - migration not needed"
        );
        return Ok(());
    }

    // Check SQLite version to determine if DROP COLUMN is supported
    let sqlite_version = sqlx::query_scalar::<_, String>("SELECT sqlite_version()")
        .fetch_one(pool)
        .await
        .map_err(|e| {
            MostroInternalErr(ServiceError::DbAccessError(format!(
                "Failed to get SQLite version: {}",
                e
            )))
        })?;

    tracing::info!("SQLite version: {}", sqlite_version);

    // Parse version to check if DROP COLUMN is supported (requires 3.35.0+)
    let supports_drop_column = sqlite_version
        .split('.')
        .take(3)
        .map(|v| v.parse::<u32>().unwrap_or(0))
        .collect::<Vec<_>>()
        .get(..3)
        .map(|parts| {
            let major = parts[0];
            let minor = parts.get(1).copied().unwrap_or(0);
            major > 3 || (major == 3 && minor >= 35)
        })
        .unwrap_or(false);

    if supports_drop_column {
        // Try DROP COLUMN approach with transaction
        tracing::info!(
            "Attempting to remove token columns using DROP COLUMN (SQLite {})...",
            sqlite_version
        );

        let mut transaction = pool.begin().await.map_err(|e| {
            MostroInternalErr(ServiceError::DbAccessError(format!(
                "Failed to begin transaction: {}",
                e
            )))
        })?;

        // Attempt to drop columns within transaction
        let drop_result = async {
            if buyer_token_exists {
                sqlx::query("ALTER TABLE disputes DROP COLUMN buyer_token")
                    .execute(&mut *transaction)
                    .await?;
                tracing::info!("Dropped buyer_token column");
            }

            if seller_token_exists {
                sqlx::query("ALTER TABLE disputes DROP COLUMN seller_token")
                    .execute(&mut *transaction)
                    .await?;
                tracing::info!("Dropped seller_token column");
            }

            Ok::<(), sqlx::Error>(())
        }
        .await;

        match drop_result {
            Ok(_) => {
                transaction.commit().await.map_err(|e| {
                    MostroInternalErr(ServiceError::DbAccessError(format!(
                        "Failed to commit transaction: {}",
                        e
                    )))
                })?;
                tracing::info!("Successfully removed token columns using DROP COLUMN");
                Ok(())
            }
            Err(e) => {
                tracing::warn!("DROP COLUMN failed ({}), falling back to table rebuild", e);
                transaction.rollback().await.map_err(|rollback_err| {
                    MostroInternalErr(ServiceError::DbAccessError(format!(
                        "Failed to rollback transaction: {}",
                        rollback_err
                    )))
                })?;

                // Fall back to table rebuild
                rebuild_disputes_table_without_tokens(pool).await
            }
        }
    } else {
        // SQLite version doesn't support DROP COLUMN, use table rebuild
        tracing::info!(
            "SQLite version {} doesn't support DROP COLUMN, using table rebuild method",
            sqlite_version
        );
        rebuild_disputes_table_without_tokens(pool).await
    }
}

async fn table_column_exists(
    pool: &SqlitePool,
    table_name: &str,
    column_name: &str,
) -> Result<bool, MostroError> {
    Ok(sqlx::query_scalar::<_, i32>(
        r#"
        SELECT COUNT(*)
        FROM pragma_table_info(?1)
        WHERE name = ?2
        "#,
    )
    .bind(table_name)
    .bind(column_name)
    .fetch_one(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
        > 0)
}

fn parse_duplicate_column_name(err: &sqlx::migrate::MigrateError) -> Option<String> {
    let error = err.to_string();
    let marker = "duplicate column name: ";
    let column = error.split(marker).nth(1)?.trim();
    Some(column.to_string())
}

fn normalize_sql_identifier(token: &str) -> String {
    token
        .trim()
        .trim_end_matches(',')
        .trim_matches('"')
        .trim_matches('`')
        .trim_matches('[')
        .trim_matches(']')
        .to_string()
}

fn strip_sql_comments(sql: &str) -> String {
    sql.lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_add_column_statements(sql: &str) -> Option<Vec<(String, String)>> {
    let sql = strip_sql_comments(sql);
    let mut operations = Vec::new();

    for statement in sql.split(';') {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }

        let tokens: Vec<_> = statement.split_whitespace().collect();
        if tokens.len() < 6
            || !tokens[0].eq_ignore_ascii_case("ALTER")
            || !tokens[1].eq_ignore_ascii_case("TABLE")
            || !tokens[3].eq_ignore_ascii_case("ADD")
            || !tokens[4].eq_ignore_ascii_case("COLUMN")
        {
            return None;
        }

        let table_name = normalize_sql_identifier(tokens[2]);
        let column_name = normalize_sql_identifier(tokens[5]);

        if table_name.is_empty() || column_name.is_empty() {
            return None;
        }

        operations.push((table_name, column_name));
    }

    if operations.is_empty() {
        None
    } else {
        Some(operations)
    }
}

async fn applied_migration_versions(pool: &SqlitePool) -> Result<Vec<i64>, MostroError> {
    sqlx::query_scalar::<_, i64>("SELECT version FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

async fn reconcile_existing_add_column_migration(
    pool: &SqlitePool,
    migrator: &sqlx::migrate::Migrator,
    duplicate_column: &str,
) -> Result<bool, MostroError> {
    let applied_versions = applied_migration_versions(pool).await?;

    for migration in migrator.iter() {
        if applied_versions.contains(&migration.version) {
            continue;
        }

        let Some(operations) = parse_add_column_statements(migration.sql.as_str()) else {
            continue;
        };

        if !operations
            .iter()
            .any(|(_, column)| column == duplicate_column)
        {
            continue;
        }

        let mut all_columns_exist = true;
        for (table_name, column_name) in &operations {
            if !table_column_exists(pool, table_name, column_name).await? {
                all_columns_exist = false;
                break;
            }
        }

        if !all_columns_exist {
            continue;
        }

        sqlx::query(
            r#"
            INSERT OR IGNORE INTO _sqlx_migrations (
                version,
                description,
                success,
                checksum,
                execution_time
            ) VALUES (?1, ?2, TRUE, ?3, 0)
            "#,
        )
        .bind(migration.version)
        .bind(&*migration.description)
        .bind(&*migration.checksum)
        .execute(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        tracing::warn!(
            version = migration.version,
            description = %migration.description,
            duplicate_column,
            "Recorded existing add-column migration as already applied"
        );

        return Ok(true);
    }

    Ok(false)
}

pub async fn connect() -> Result<Arc<Pool<Sqlite>>, MostroError> {
    // Get mostro settings
    let db_settings = Settings::get_db();
    let db_url = &db_settings.url;
    let tmp = db_url.replace("sqlite://", "");
    let db_path = Path::new(&tmp);

    let conn = if !db_path.exists() {
        // Create new database file
        let _file = std::fs::File::create_new(db_path)
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        // Restrict file permissions — only owner can read and write
        #[cfg(unix)]
        {
            set_permissions(db_path, Permissions::from_mode(0o600))
                .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        }

        // Create new database connection
        match SqlitePool::connect(db_url).await {
            Ok(pool) => {
                match sqlx::migrate!().run(&pool).await {
                    Ok(_) => {
                        tracing::info!(
                            "Successfully created database file at {}",
                            db_path.display(),
                        );

                        // Run legacy column migration
                        if let Err(e) = migrate_remove_token_columns(&pool).await {
                            tracing::error!("Failed to migrate token columns: {}", e);
                            if let Err(cleanup_err) = std::fs::remove_file(db_path) {
                                tracing::error!(
                                    error = %cleanup_err,
                                    path = %db_path.display(),
                                    "Failed to clean up database file"
                                );
                            }
                            return Err(e);
                        }

                        pool
                    }
                    Err(e) => {
                        if let Err(cleanup_err) = std::fs::remove_file(db_path) {
                            tracing::error!(
                                error = %cleanup_err,
                                path = %db_path.display(),
                                "Failed to clean up database file"
                            );
                        }
                        return Err(MostroInternalErr(ServiceError::DbAccessError(
                            e.to_string(),
                        )));
                    }
                }
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
        // Connect to existing database
        let conn = SqlitePool::connect(db_url)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        // Run migrations for existing databases too
        let migrator = sqlx::migrate!();
        if let Err(e) = migrator.run(&conn).await {
            if let Some(duplicate_column) = parse_duplicate_column_name(&e) {
                if reconcile_existing_add_column_migration(&conn, &migrator, &duplicate_column)
                    .await?
                {
                    if let Err(e) = migrator.run(&conn).await {
                        tracing::error!("Failed to run migrations on existing database: {}", e);
                        return Err(MostroInternalErr(ServiceError::DbAccessError(
                            e.to_string(),
                        )));
                    }
                } else {
                    tracing::error!("Failed to run migrations on existing database: {}", e);
                    return Err(MostroInternalErr(ServiceError::DbAccessError(
                        e.to_string(),
                    )));
                }
            } else {
                tracing::error!("Failed to run migrations on existing database: {}", e);
                return Err(MostroInternalErr(ServiceError::DbAccessError(
                    e.to_string(),
                )));
            }
        }

        // Run legacy column migration for existing databases
        if let Err(e) = migrate_remove_token_columns(&conn).await {
            tracing::error!(
                "Failed to migrate token columns on existing database: {}",
                e
            );
            return Err(e);
        }

        conn
    };
    Ok(Arc::new(conn))
}

/// Retrieve the stored admin password hash from the users table.
pub async fn get_admin_password(pool: &SqlitePool) -> Result<Option<String>, MostroError> {
    if let Some(user) = sqlx::query_as::<_, User>(
        r#"
          SELECT *
          FROM users
          WHERE is_admin == 1
          LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await
    .map_err(|_| {
        MostroInternalErr(ServiceError::DbAccessError(
            "Failed to get admin password".to_string(),
        ))
    })? {
        Ok(user.admin_password)
    } else {
        Ok(None)
    }
}

pub async fn edit_pubkeys_order(pool: &SqlitePool, order: &Order) -> Result<Order, MostroError> {
    let null_key = None::<String>;
    let result = match order.get_order_kind() {
        Ok(OrderKind::Buy) => {
            sqlx::query(
                "UPDATE orders SET seller_pubkey = ?1, master_seller_pubkey = ?2 WHERE id = ?3",
            )
            .bind(null_key.clone())
            .bind(null_key)
            .bind(order.id)
            .execute(pool)
            .await
        }
        Ok(OrderKind::Sell) => {
            sqlx::query(
                "UPDATE orders SET buyer_pubkey = ?1, master_buyer_pubkey = ?2 WHERE id = ?3",
            )
            .bind(null_key.clone())
            .bind(null_key)
            .bind(order.id)
            .execute(pool)
            .await
        }
        Err(_) => {
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                "Order kind not found".to_string(),
            )));
        }
    }
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if result.rows_affected() == 0 {
        return Err(MostroInternalErr(ServiceError::DbAccessError(
            "No order updated".to_string(),
        )));
    }

    // Return the updated order
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE id = ?1
        "#,
    )
    .bind(order.id)
    .fetch_one(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(order)
}

/// Returns orders with `failed_payment = true` and `status = "settled-hold-invoice"`
/// for the given buyer's `master_buyer_pubkey`. Used during restore-session to
/// re-send `Action::AddInvoice` to buyers whose Lightning payment failed.
pub async fn find_failed_payment_for_master_key(
    pool: &SqlitePool,
    master_key: &str,
) -> Result<Vec<Order>, MostroError> {
    // Validate public key format (32-bytes hex)
    if !master_key.chars().all(|c| c.is_ascii_hexdigit()) || master_key.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }
    let orders = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE failed_payment = true
            AND status = 'settled-hold-invoice'
            AND master_buyer_pubkey = ?1
        "#,
    )
    .bind(master_key)
    .fetch_all(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    Ok(orders)
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
    // Phase 1.5: `waiting-taker-bond` is a daemon-internal pre-trade
    // status (a prospective taker is mid-bond). On the wire it publishes
    // as `pending`, so from the orderbook's perspective both buckets are
    // equivalent — and so the expiry job must cover both. Without
    // `waiting-taker-bond` here, an order parked at that status past its
    // `expires_at` would never expire and the bond HTLCs would tie up
    // taker funds in LND until CLTV expiry.
    //
    // Phase 5: `waiting-maker-bond` is the maker-side analogue — an order
    // whose maker never paid the bond, so it was never published to
    // Nostr at all. It must also expire here, otherwise the abandoned
    // order row and its bond HTLC linger until CLTV. Unlike the other two
    // buckets this status has no NIP-33 event, so the expiry job skips the
    // Nostr republish for it (see `job_expire_pending_older_orders`).
    let order = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE expires_at < ?1
            AND status IN ('pending', 'waiting-taker-bond', 'waiting-maker-bond')
        "#,
    )
    .bind(expire_time.as_secs() as i64)
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
    .bind(expire_time.as_secs() as i64)
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
    dev_fee: i64,
) -> Result<bool, MostroError> {
    let status = Status::Pending.to_string();
    let hash: Option<String> = None;
    let preimage: Option<String> = None;
    let buyer_invoice: Option<String> = None;

    let result = sqlx::query(
        r#"
            UPDATE orders
            SET
            status = ?1,
            amount = ?2,
            fee = ?3,
            dev_fee = ?4,
            hash = ?5,
            preimage = ?6,
            buyer_invoice = ?7,
            taken_at = ?8,
            invoice_held_at = ?9
            WHERE id = ?10
        "#,
    )
    .bind(status)
    .bind(amount)
    .bind(fee)
    .bind(dev_fee)
    .bind(hash)
    .bind(preimage)
    .bind(buyer_invoice)
    .bind(0_i64)
    .bind(0_i64)
    .bind(order_id)
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

pub async fn reset_order_taken_at_time(
    pool: &SqlitePool,
    order_id: Uuid,
) -> Result<bool, MostroError> {
    let taken_at = 0_i64;
    let result = sqlx::query(
        r#"
            UPDATE orders
            SET
            taken_at = ?1
            WHERE id = ?2
        "#,
    )
    .bind(taken_at)
    .bind(order_id)
    .execute(pool)
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
    let result = sqlx::query(
        r#"
            UPDATE orders
            SET
            invoice_held_at = ?1
            WHERE id = ?2
        "#,
    )
    .bind(invoice_held_at)
    .bind(order_id)
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

/// Atomically persist a validated Cashu escrow and advance the order status
/// (Cashu foundation CF-4, `docs/cashu/01-fundamentals.md` §6).
///
/// Compare-and-set: the three `cashu_*` columns and the status transition
/// are written in one `UPDATE … WHERE id = ? AND status = ? AND
/// cashu_escrow_locked_at IS NULL`, so there is no lock-without-advance
/// window, and a replayed or concurrent submission (already locked, or the
/// status moved on) matches zero rows instead of double-writing. Returns
/// whether a row matched.
pub async fn update_order_cashu_escrow(
    pool: &SqlitePool,
    order_id: Uuid,
    mint_url: &str,
    token: &str,
    locked_at: i64,
    expected_status: Status,
    new_status: Status,
) -> Result<bool, MostroError> {
    let result = sqlx::query(
        r#"
            UPDATE orders
            SET
            cashu_mint_url = ?1,
            cashu_escrow_token = ?2,
            cashu_escrow_locked_at = ?3,
            status = ?4
            WHERE id = ?5 AND status = ?6 AND cashu_escrow_locked_at IS NULL
        "#,
    )
    .bind(mint_url)
    .bind(token)
    .bind(locked_at)
    .bind(new_status.to_string())
    .bind(order_id)
    .bind(expected_status.to_string())
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(result.rows_affected() > 0)
}

/// Every order with a locked Cashu escrow, for restore/monitoring after a
/// restart (CF-4). Deliberately **no** `status` predicate:
/// [`update_order_cashu_escrow`] advances the status in the same write as
/// the lock, so filtering on a status would skip legitimately locked rows
/// that have already moved on. Only re-add a status predicate if a separate
/// invariant guarantees locked rows never leave that status.
pub async fn find_locked_cashu_orders(pool: &SqlitePool) -> Result<Vec<Order>, MostroError> {
    sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE cashu_escrow_locked_at IS NOT NULL
          ORDER BY cashu_escrow_locked_at ASC
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
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

pub async fn find_unpaid_dev_fees(pool: &SqlitePool) -> Result<Vec<Order>, MostroError> {
    let orders = sqlx::query_as::<_, Order>(
        r#"
          SELECT *
          FROM orders
          WHERE (status = 'settled-hold-invoice' OR status = 'success')
            AND dev_fee > 0
            AND dev_fee_paid = 0
            AND (dev_fee_payment_hash IS NULL OR dev_fee_payment_hash = '')
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(orders)
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
    let created_at: Timestamp = Timestamp::now();
    let _result = sqlx::query(
        "
            INSERT INTO users (pubkey, is_admin,admin_password, is_solver, is_banned, category, last_trade_index, total_reviews, total_rating, last_rating, max_rating, min_rating, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        ",
    )
    .bind(new_user.pubkey.clone())
    .bind(new_user.is_admin)
    .bind(new_user.admin_password)
    .bind(new_user.is_solver)
    .bind(new_user.is_banned)
    .bind(new_user.category)
    .bind(new_user.last_trade_index)
    .bind(new_user.total_reviews)
    .bind(new_user.total_rating)
    .bind(new_user.last_rating)
    .bind(new_user.max_rating)
    .bind(new_user.min_rating)
    .bind(created_at.as_secs() as i64)
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Return the pubkey as stored (plain)
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

    let result = sqlx::query(
        r#"
            UPDATE users SET last_trade_index = ?1 WHERE pubkey = ?2
        "#,
    )
    .bind(trade_index)
    .bind(public_key)
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

pub async fn buyer_has_pending_order(
    pool: &SqlitePool,
    pubkey: String,
) -> Result<bool, MostroError> {
    has_pending_order_with_status(pool, pubkey, "master_buyer_pubkey", "waiting-buyer-invoice")
        .await
}

pub async fn seller_has_pending_order(
    pool: &SqlitePool,
    pubkey: String,
) -> Result<bool, MostroError> {
    has_pending_order_with_status(pool, pubkey, "master_seller_pubkey", "waiting-payment").await
}

async fn has_pending_order_with_status(
    pool: &SqlitePool,
    pubkey: String,
    master_key_field: &str,
    status: &str,
) -> Result<bool, MostroError> {
    // Validate public key format (32-bytes hex)
    if !pubkey.chars().all(|c| c.is_ascii_hexdigit()) || pubkey.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    let exists = match master_key_field {
        "master_buyer_pubkey" => {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM orders WHERE master_buyer_pubkey = ? AND status = ?)",
            )
            .bind(pubkey)
            .bind(status)
            .fetch_one(pool)
            .await
        }
        "master_seller_pubkey" => sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM orders WHERE master_seller_pubkey = ? AND status = ?)",
        )
        .bind(pubkey)
        .bind(status)
        .fetch_one(pool)
        .await,
        _ => {
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                "Invalid master key field".to_string(),
            )));
        }
    }
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    Ok(exists)
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
    let result = sqlx::query(
        r#"
            UPDATE users SET last_rating = ?1, min_rating = ?2, max_rating = ?3, total_reviews = ?4, total_rating = ?5 WHERE pubkey = ?6
        "#,
    )
    .bind(last_rating)
    .bind(min_rating)
    .bind(max_rating)
    .bind(total_reviews)
    .bind(total_rating)
    .bind(public_key)
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let rows_affected = result.rows_affected();

    Ok(rows_affected > 0)
}

/// Returns true only when the given `solver_pubkey` is assigned to the dispute
/// identified by `order_id` (`disputes.solver_pubkey` + `disputes.order_id`) and
/// the matching user row is a solver with read-write permission
/// (`users.is_solver = true` and `users.category = 2`).
pub async fn solver_has_write_permission(
    pool: &SqlitePool,
    solver_pubkey: &str,
    order_id: Uuid,
) -> Result<bool, MostroError> {
    let result = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM disputes d
            INNER JOIN users u ON u.pubkey = d.solver_pubkey
            WHERE d.solver_pubkey = ?1
              AND d.order_id = ?2
              AND u.is_solver = true
              AND u.category = 2
        )
        "#,
    )
    .bind(solver_pubkey)
    .bind(order_id)
    .fetch_one(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(result)
}

/// Ensures the caller may finalize a dispute (`admin-settle` / `admin-cancel`).
///
/// Requires the caller to be the assigned solver (`is_assigned_solver`). The
/// Mostro daemon identity (`caller_pubkey == admin_pubkey`) then bypasses solver
/// category checks, matching `admin_take_dispute`. Human solvers must also have
/// read-write permission on the assigned dispute.
pub async fn ensure_dispute_finalize_permission(
    pool: &SqlitePool,
    caller_pubkey: &str,
    admin_pubkey: &str,
    order_id: Uuid,
) -> Result<(), MostroError> {
    if !is_assigned_solver(pool, caller_pubkey, order_id).await? {
        return Err(MostroCantDo(CantDoReason::IsNotYourDispute));
    }
    if caller_pubkey == admin_pubkey {
        return Ok(());
    }
    if solver_has_write_permission(pool, caller_pubkey, order_id).await? {
        Ok(())
    } else {
        Err(MostroCantDo(CantDoReason::NotAuthorized))
    }
}

/// Returns true when `pubkey` corresponds to a solver user with read-write
/// permission (`users.is_solver = true` and `users.category = 2`), independent
/// of any dispute assignment. Use this when the caller is a prospective taker
/// rather than the currently assigned solver.
pub async fn user_has_solver_write_permission(
    pool: &SqlitePool,
    pubkey: &str,
) -> Result<bool, MostroError> {
    let result = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM users
            WHERE pubkey = ?1
              AND is_solver = true
              AND category = 2
        )
        "#,
    )
    .bind(pubkey)
    .fetch_one(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(result)
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

/// Check if a dispute has been taken over by admin (Mostro daemon)
/// This helps provide better error messages when solver tries to act on admin-taken disputes
pub async fn is_dispute_taken_by_admin(
    pool: &SqlitePool,
    order_id: Uuid,
    admin_pubkey: &str,
) -> Result<bool, MostroError> {
    // Get the dispute for this order
    let dispute = sqlx::query(
        "SELECT solver_pubkey FROM disputes WHERE order_id = ? AND status = 'in-progress'",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if let Some(row) = dispute {
        if let Some(solver_pubkey) = row
            .try_get::<Option<String>, _>("solver_pubkey")
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
        {
            // Check if the current solver is the admin (mostro daemon)
            return Ok(solver_pubkey == admin_pubkey);
        }
    }

    Ok(false)
}

/// Find all orders for a user by their master key (for restore session).
/// Uses constants for excluded statuses to maintain consistency across queries.
pub async fn find_user_orders_by_master_key(
    pool: &SqlitePool,
    master_key: &str,
) -> Result<Vec<RestoredOrdersInfo>, MostroError> {
    // Validate public key format (32-bytes hex)
    if !master_key.chars().all(|c| c.is_ascii_hexdigit()) || master_key.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    let sql_query = format!(
        r#"
        SELECT id as order_id, trade_index_buyer as trade_index, status FROM orders 
        WHERE (master_buyer_pubkey = ?)
        AND status NOT IN ({})
        UNION ALL
        SELECT id as order_id, trade_index_seller as trade_index, status FROM orders 
        WHERE (master_seller_pubkey = ?)
        AND status NOT IN ({})
        "#,
        EXCLUDED_ORDER_STATUSES, EXCLUDED_ORDER_STATUSES
    );
    let orders = sqlx::query_as::<_, RestoredOrdersInfo>(AssertSqlSafe(sql_query))
        .bind(master_key)
        .bind(master_key)
        .fetch_all(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(orders)
}

/// Find all disputes for a user by their master key (for restore session)
pub async fn find_user_disputes_by_master_key(
    pool: &SqlitePool,
    master_key: &str,
) -> Result<Vec<RestoredDisputesInfo>, MostroError> {
    // Validate public key format (32-bytes hex)
    if !master_key.chars().all(|c| c.is_ascii_hexdigit()) || master_key.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    let sql_query = format!(
        r#"
        SELECT
            d.id AS dispute_id,
            d.order_id AS order_id,
            COALESCE(
                CASE
                    WHEN o.master_buyer_pubkey = ? THEN o.trade_index_buyer
                    WHEN o.master_seller_pubkey = ? THEN o.trade_index_seller
                    ELSE 0
                END, 0
            ) AS trade_index,
            d.status AS status,
            CASE
                WHEN o.buyer_dispute = 1 AND o.seller_dispute = 0 THEN 'buyer'
                WHEN o.seller_dispute = 1 AND o.buyer_dispute = 0 THEN 'seller'
                ELSE NULL
            END AS initiator,
            d.solver_pubkey AS solver_pubkey
        FROM disputes d
        JOIN orders o ON d.order_id = o.id
        WHERE (o.master_buyer_pubkey = ? OR o.master_seller_pubkey = ?)
            AND d.status IN ({})
        "#,
        ACTIVE_DISPUTE_STATUSES
    );
    let restore_disputes = sqlx::query_as::<_, RestoredDisputesInfo>(AssertSqlSafe(sql_query))
        //CASE
        .bind(master_key)
        .bind(master_key)
        //WHERE
        .bind(master_key)
        .bind(master_key)
        .fetch_all(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(restore_disputes)
}

/// The actual work function that runs the restore session query.
async fn process_restore_session_work(
    pool: SqlitePool,
    master_key: String,
) -> Result<RestoreSessionInfo, MostroError> {
    // Find all active orders for this user
    let restore_orders = find_user_orders_by_master_key(&pool, &master_key).await?;
    // Find all active disputes for this user
    let restore_disputes = find_user_disputes_by_master_key(&pool, &master_key).await?;

    tracing::info!(
        "Background restore session completed with {} orders, {} disputes",
        restore_orders.len(),
        restore_disputes.len()
    );

    Ok(RestoreSessionInfo {
        restore_orders,
        restore_disputes,
    })
}

/// Background task manager for restore sessions
pub struct RestoreSessionManager {
    sender: tokio::sync::mpsc::Sender<RestoreSessionInfo>,
    receiver: tokio::sync::mpsc::Receiver<RestoreSessionInfo>,
}

impl Default for RestoreSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RestoreSessionManager {
    pub fn new() -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(10);
        Self { sender, receiver }
    }

    /// Start a restore session background task
    pub async fn start_restore_session(
        &self,
        pool: SqlitePool,
        master_key: String,
    ) -> Result<(), MostroError> {
        let sender = self.sender.clone();

        // Use spawn_blocking to avoid blocking the async runtime
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            match handle.block_on(process_restore_session_work(pool, master_key)) {
                Ok(restore_data) => {
                    // No need for an async context just to send; this is a blocking thread.
                    if let Err(e) = sender.blocking_send(restore_data) {
                        tracing::warn!(
                            "RestoreSessionManager: receiver dropped before sending result: {}",
                            e
                        );
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to process restore session work: {}", e);
                }
            }
        });

        Ok(())
    }

    /// Check for completed restore session results
    pub async fn check_results(&mut self) -> Option<RestoreSessionInfo> {
        self.receiver.try_recv().ok()
    }

    /// Wait for the next restore session result
    pub async fn wait_for_result(&mut self) -> Option<RestoreSessionInfo> {
        self.receiver.recv().await
    }
}

// Add this cfg attribute if the code is *only* for testing
#[cfg(test)]
mod tests {
    use mostro_core::error::CantDoReason;
    use mostro_core::prelude::MostroError;
    use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
    use sqlx::{AssertSqlSafe, Error};
    use std::collections::HashSet;

    const TEST_DB_URL: &str = "sqlite::memory:";

    // Helper function to set up the database and pool
    async fn setup_db() -> Result<SqlitePool, Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1) // Usually fine for simple tests
            .connect(TEST_DB_URL)
            .await?;

        // Create the table
        sqlx::query(
            r#"
            CREATE TABLE items (
                id INTEGER PRIMARY KEY,
                value TEXT NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await?;

        Ok(pool)
    }

    /// Create the orders table matching the production schema (base + dev_fee migration)
    async fn setup_orders_db() -> Result<SqlitePool, Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(TEST_DB_URL)
            .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS orders (
                id char(36) primary key not null,
                kind varchar(4) not null,
                event_id char(64) not null,
                hash char(64),
                preimage char(64),
                creator_pubkey char(64),
                cancel_initiator_pubkey char(64),
                dispute_initiator_pubkey char(64),
                buyer_pubkey char(64),
                master_buyer_pubkey char(64),
                seller_pubkey char(64),
                master_seller_pubkey char(64),
                status varchar(50) not null,
                price_from_api integer not null default 0,
                premium integer not null,
                payment_method varchar(500) not null,
                amount integer not null,
                min_amount integer default 0,
                max_amount integer default 0,
                buyer_dispute integer not null default 0,
                seller_dispute integer not null default 0,
                buyer_cooperativecancel integer not null default 0,
                seller_cooperativecancel integer not null default 0,
                fee integer not null default 0,
                routing_fee integer not null default 0,
                fiat_code varchar(5) not null,
                fiat_amount integer not null,
                buyer_invoice text,
                range_parent_id char(36),
                invoice_held_at integer default 0,
                taken_at integer default 0,
                created_at integer not null,
                buyer_sent_rate integer default 0,
                seller_sent_rate integer default 0,
                payment_attempts integer default 0,
                failed_payment integer default 0,
                expires_at integer not null,
                trade_index_seller integer default 0,
                trade_index_buyer integer default 0,
                next_trade_pubkey char(64),
                next_trade_index integer default 0,
                dev_fee integer default 0,
                dev_fee_paid integer not null default 0,
                dev_fee_payment_hash char(64),
                cashu_mint_url text,
                cashu_escrow_token text,
                cashu_escrow_locked_at integer
            )
            "#,
        )
        .execute(&pool)
        .await?;

        Ok(pool)
    }

    /// Insert a minimal test order with the fields relevant to order status and dev fee queries.
    /// Binds `id` as `Uuid` so storage format matches production queries.
    async fn insert_test_order(
        pool: &SqlitePool,
        id: uuid::Uuid,
        status: &str,
        dev_fee: i64,
        dev_fee_paid: bool,
        dev_fee_payment_hash: Option<&str>,
    ) {
        sqlx::query(
            r#"
            INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                                amount, fiat_code, fiat_amount, created_at, expires_at,
                                failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                                dev_fee_payment_hash)
            VALUES (?1, 'buy', 'event123', ?2, 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, ?3, ?4, ?5)
            "#,
        )
        .bind(id)
        .bind(status)
        .bind(dev_fee)
        .bind(dev_fee_paid)
        .bind(dev_fee_payment_hash)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Insert an order carrying explicit trade pubkeys, for the Phase 2
    /// active-trade-pubkey cache query.
    async fn insert_order_with_pubkeys(
        pool: &SqlitePool,
        id: uuid::Uuid,
        status: &str,
        creator: Option<&str>,
        buyer: Option<&str>,
        seller: Option<&str>,
    ) {
        sqlx::query(
            r#"
            INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                                amount, fiat_code, fiat_amount, created_at, expires_at,
                                creator_pubkey, buyer_pubkey, seller_pubkey)
            VALUES (?1, 'buy', 'event123', ?2, 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400, ?3, ?4, ?5)
            "#,
        )
        .bind(id)
        .bind(status)
        .bind(creator)
        .bind(buyer)
        .bind(seller)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn setup_disputes_table(pool: &SqlitePool) {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS disputes (
                id char(36) primary key not null,
                order_id char(36) unique not null,
                status varchar(10) not null,
                order_previous_status varchar(10) not null,
                solver_pubkey char(64),
                created_at integer not null,
                taken_at integer default 0
            )
            "#,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// Phase 2 (docs/TRANSPORT_V2_SPEC.md §6): the active-trade-pubkey cache
    /// must include participants of every non-terminal order — **including
    /// disputed ones** — and the solvers of active disputes, while excluding
    /// terminal orders and resolved disputes.
    #[tokio::test]
    async fn find_active_trade_pubkeys_skips_empty_string_pubkeys() {
        // A present-but-empty pubkey column (`Some("")`) must be skipped, not
        // inserted as a zero-length "active key" — exercises the
        // `if !pk.is_empty()` guard for both the orders and disputes loops.
        let pool = setup_orders_db().await.unwrap();
        setup_disputes_table(&pool).await;

        insert_order_with_pubkeys(
            &pool,
            uuid::Uuid::new_v4(),
            "waiting-payment",
            Some("creator_only"),
            Some(""), // empty buyer pubkey → skipped
            Some(""), // empty seller pubkey → skipped
        )
        .await;
        // Active dispute whose solver pubkey is the empty string → skipped.
        sqlx::query(
            "INSERT INTO disputes (id, order_id, status, order_previous_status, solver_pubkey, created_at) \
             VALUES (?1, ?2, 'in-progress', 'fiat-sent', '', 1700000000)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

        let keys: HashSet<String> = super::find_active_trade_pubkeys(&pool)
            .await
            .unwrap()
            .into_iter()
            .collect();

        assert!(keys.contains("creator_only"));
        assert!(
            !keys.contains(""),
            "empty-string pubkeys must never be treated as active keys"
        );
        assert_eq!(keys.len(), 1, "only the non-empty creator key is active");
    }

    #[tokio::test]
    async fn find_active_trade_pubkeys_covers_active_and_disputed_excludes_terminal() {
        let pool = setup_orders_db().await.unwrap();
        setup_disputes_table(&pool).await;

        // Active order — all three keys are "known".
        insert_order_with_pubkeys(
            &pool,
            uuid::Uuid::new_v4(),
            "waiting-payment",
            Some("creator_active"),
            Some("buyer_active"),
            Some("seller_active"),
        )
        .await;
        // Disputed order — still active (the load-bearing nuance: 'dispute' is
        // NOT in TERMINAL_ORDER_STATUSES), so its keys must be included.
        insert_order_with_pubkeys(
            &pool,
            uuid::Uuid::new_v4(),
            "dispute",
            Some("creator_disp"),
            Some("buyer_disp"),
            Some("seller_disp"),
        )
        .await;
        // Terminal orders — excluded.
        insert_order_with_pubkeys(
            &pool,
            uuid::Uuid::new_v4(),
            "success",
            Some("creator_succ"),
            Some("buyer_succ"),
            Some("seller_succ"),
        )
        .await;
        insert_order_with_pubkeys(
            &pool,
            uuid::Uuid::new_v4(),
            "canceled",
            Some("creator_canc"),
            None,
            None,
        )
        .await;

        // Active dispute with an assigned solver → solver key included.
        sqlx::query(
            "INSERT INTO disputes (id, order_id, status, order_previous_status, solver_pubkey, created_at) \
             VALUES (?1, ?2, 'in-progress', 'fiat-sent', 'solver_active', 1700000000)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
        // Resolved dispute → its solver is NOT included.
        sqlx::query(
            "INSERT INTO disputes (id, order_id, status, order_previous_status, solver_pubkey, created_at) \
             VALUES (?1, ?2, 'settled', 'fiat-sent', 'solver_settled', 1700000000)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

        let keys: HashSet<String> = super::find_active_trade_pubkeys(&pool)
            .await
            .unwrap()
            .into_iter()
            .collect();

        for k in [
            "creator_active",
            "buyer_active",
            "seller_active",
            "creator_disp",
            "buyer_disp",
            "seller_disp",
            "solver_active",
        ] {
            assert!(keys.contains(k), "{k} should be a known active key");
        }
        for k in [
            "creator_succ",
            "buyer_succ",
            "seller_succ",
            "creator_canc",
            "solver_settled",
        ] {
            assert!(
                !keys.contains(k),
                "{k} must NOT be known (terminal/resolved)"
            );
        }
    }

    #[tokio::test]
    async fn test_fetch_string_column_scalar() {
        let pool = setup_db().await.unwrap();

        let total_entries = 20;
        let mut query_builder = String::from("INSERT INTO items (id, value) VALUES ");
        let mut params: Vec<String> = Vec::new();

        for i in 0..total_entries {
            let value_string = format!("Entry {}", i % 5);
            if i > 0 {
                query_builder.push_str(", ");
            }
            query_builder.push_str(&format!("({}, ?)", i));
            params.push(value_string);
        }

        let mut query = sqlx::query(AssertSqlSafe(query_builder));
        for param in &params {
            query = query.bind(param);
        }
        query.execute(&pool).await.unwrap();

        let sql = "SELECT value FROM items ORDER BY id";
        let fetched_values: Vec<String> = sqlx::query_scalar(sql).fetch_all(&pool).await.unwrap();

        let hash_set_values: HashSet<String> = fetched_values.into_iter().collect();
        assert!(
            hash_set_values.contains("Entry 0"),
            "Should contain Entry 0"
        );
        assert!(
            hash_set_values.contains("Entry 1"),
            "Should contain Entry 1"
        );
        assert!(
            hash_set_values.contains("Entry 2"),
            "Should contain Entry 2"
        );
        assert!(
            hash_set_values.contains("Entry 3"),
            "Should contain Entry 3"
        );
        assert!(
            hash_set_values.contains("Entry 4"),
            "Should contain Entry 4"
        );
        assert_eq!(
            hash_set_values.len(),
            5,
            "Should have exactly 5 unique entries"
        );
    }

    #[tokio::test]
    async fn find_unpaid_dev_fees_returns_eligible_orders() {
        let pool = setup_orders_db().await.unwrap();
        let id1 = uuid::Uuid::new_v4();
        let id2 = uuid::Uuid::new_v4();

        // Eligible: success status, dev_fee > 0, not paid, no hash
        insert_test_order(&pool, id1, "success", 100, false, None).await;
        // Also eligible: settled-hold-invoice status
        insert_test_order(&pool, id2, "settled-hold-invoice", 50, false, None).await;

        let result = super::find_unpaid_dev_fees(&pool).await.unwrap();
        assert_eq!(result.len(), 2, "Should find both eligible orders");
    }

    #[tokio::test]
    async fn find_unpaid_dev_fees_excludes_already_paid() {
        let pool = setup_orders_db().await.unwrap();

        insert_test_order(&pool, uuid::Uuid::new_v4(), "success", 100, true, None).await;

        let result = super::find_unpaid_dev_fees(&pool).await.unwrap();
        assert!(result.is_empty(), "Should not return already-paid orders");
    }

    #[tokio::test]
    async fn find_unpaid_dev_fees_excludes_orders_with_existing_hash() {
        let pool = setup_orders_db().await.unwrap();

        // Has existing payment hash (in-flight or pending)
        insert_test_order(
            &pool,
            uuid::Uuid::new_v4(),
            "success",
            100,
            false,
            Some("abc123hash"),
        )
        .await;
        // Has PENDING marker
        insert_test_order(
            &pool,
            uuid::Uuid::new_v4(),
            "success",
            100,
            false,
            Some("PENDING-uuid-123"),
        )
        .await;

        let result = super::find_unpaid_dev_fees(&pool).await.unwrap();
        assert!(
            result.is_empty(),
            "Should not return orders with existing payment hash"
        );
    }

    #[tokio::test]
    async fn find_unpaid_dev_fees_excludes_wrong_status() {
        let pool = setup_orders_db().await.unwrap();

        insert_test_order(&pool, uuid::Uuid::new_v4(), "active", 100, false, None).await;
        insert_test_order(&pool, uuid::Uuid::new_v4(), "pending", 100, false, None).await;
        insert_test_order(&pool, uuid::Uuid::new_v4(), "expired", 100, false, None).await;

        let result = super::find_unpaid_dev_fees(&pool).await.unwrap();
        assert!(
            result.is_empty(),
            "Should not return orders with non-eligible statuses"
        );
    }

    #[tokio::test]
    async fn find_unpaid_dev_fees_excludes_zero_dev_fee() {
        let pool = setup_orders_db().await.unwrap();

        insert_test_order(&pool, uuid::Uuid::new_v4(), "success", 0, false, None).await;

        let result = super::find_unpaid_dev_fees(&pool).await.unwrap();
        assert!(
            result.is_empty(),
            "Should not return orders with zero dev_fee"
        );
    }

    #[tokio::test]
    async fn find_unpaid_dev_fees_with_empty_hash_string() {
        let pool = setup_orders_db().await.unwrap();

        // Empty string hash (should be treated same as NULL)
        insert_test_order(&pool, uuid::Uuid::new_v4(), "success", 100, false, Some("")).await;

        let result = super::find_unpaid_dev_fees(&pool).await.unwrap();
        assert_eq!(
            result.len(),
            1,
            "Empty string hash should be treated as no hash"
        );
    }

    // -- Tests for find_held_invoices --

    #[tokio::test]
    async fn test_find_held_invoices_returns_active_with_held_at() {
        let pool = setup_orders_db().await.unwrap();
        let id = uuid::Uuid::new_v4();

        // Insert order with invoice_held_at != 0 and status = 'active'
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    invoice_held_at)
            VALUES (?1, 'buy', 'ev1', 'active', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, 1700001000)"#,
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        let result = super::find_held_invoices(&pool).await.unwrap();
        assert_eq!(
            result.len(),
            1,
            "Should find active order with held invoice"
        );
    }

    #[tokio::test]
    async fn test_find_held_invoices_ignores_non_active() {
        let pool = setup_orders_db().await.unwrap();

        // Insert order with invoice_held_at != 0 but wrong status
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    invoice_held_at)
            VALUES (?1, 'buy', 'ev1', 'pending', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, 1700001000)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

        let result = super::find_held_invoices(&pool).await.unwrap();
        assert!(result.is_empty(), "Should not find non-active orders");
    }

    #[tokio::test]
    async fn test_find_held_invoices_ignores_zero_held_at() {
        let pool = setup_orders_db().await.unwrap();

        // Insert active order but invoice_held_at = 0
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    invoice_held_at)
            VALUES (?1, 'buy', 'ev1', 'active', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, 0)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

        let result = super::find_held_invoices(&pool).await.unwrap();
        assert!(result.is_empty(), "Should not find orders with held_at = 0");
    }

    // -- Tests for find_failed_payment --

    #[tokio::test]
    async fn test_find_failed_payment_returns_matching() {
        let pool = setup_orders_db().await.unwrap();

        // Insert order with failed_payment = true and status = settled-hold-invoice
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid)
            VALUES (?1, 'buy', 'ev1', 'settled-hold-invoice', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    1, 3, 0, 0)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

        let result = super::find_failed_payment(&pool).await.unwrap();
        assert_eq!(result.len(), 1, "Should find failed payment order");
    }

    #[tokio::test]
    async fn test_find_failed_payment_ignores_non_failed() {
        let pool = setup_orders_db().await.unwrap();

        // Insert order with failed_payment = false
        insert_test_order(
            &pool,
            uuid::Uuid::new_v4(),
            "settled-hold-invoice",
            0,
            false,
            None,
        )
        .await;

        let result = super::find_failed_payment(&pool).await.unwrap();
        assert!(result.is_empty(), "Should not find non-failed orders");
    }

    #[tokio::test]
    async fn test_find_failed_payment_ignores_wrong_status() {
        let pool = setup_orders_db().await.unwrap();

        // Insert order with failed_payment = true but wrong status
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid)
            VALUES (?1, 'buy', 'ev1', 'active', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    1, 3, 0, 0)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();

        let result = super::find_failed_payment(&pool).await.unwrap();
        assert!(
            result.is_empty(),
            "Should not find orders with wrong status"
        );
    }

    // -- Tests for find_failed_payment_for_master_key --
    #[tokio::test]
    async fn test_find_failed_payment_for_master_key_returns_matching() {
        let pool = setup_orders_db().await.unwrap();
        let master_key = "a".repeat(64);
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_buyer_pubkey)
            VALUES (?1, 'buy', 'ev1', 'settled-hold-invoice', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    1, 3, 0, 0, ?2)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(&master_key)
        .execute(&pool)
        .await
        .unwrap();
        let result = super::find_failed_payment_for_master_key(&pool, &master_key)
            .await
            .unwrap();
        assert_eq!(result.len(), 1, "Should find matching failed payment order");
    }

    #[tokio::test]
    async fn test_find_failed_payment_for_master_key_ignores_different_key() {
        let pool = setup_orders_db().await.unwrap();
        let master_key = "a".repeat(64);
        let other_key = "b".repeat(64);
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_buyer_pubkey)
            VALUES (?1, 'buy', 'ev1', 'settled-hold-invoice', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    1, 3, 0, 0, ?2)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(&other_key)
        .execute(&pool)
        .await
        .unwrap();
        let result = super::find_failed_payment_for_master_key(&pool, &master_key)
            .await
            .unwrap();
        assert!(
            result.is_empty(),
            "Should not return orders for a different master key"
        );
    }

    #[tokio::test]
    async fn test_find_failed_payment_for_master_key_ignores_non_failed() {
        let pool = setup_orders_db().await.unwrap();
        let master_key = "a".repeat(64);
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_buyer_pubkey)
            VALUES (?1, 'buy', 'ev1', 'settled-hold-invoice', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?2)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(&master_key)
        .execute(&pool)
        .await
        .unwrap();
        let result = super::find_failed_payment_for_master_key(&pool, &master_key)
            .await
            .unwrap();
        assert!(
            result.is_empty(),
            "Should not return orders where failed_payment is false"
        );
    }

    #[tokio::test]
    async fn test_find_failed_payment_for_master_key_ignores_wrong_status() {
        let pool = setup_orders_db().await.unwrap();
        let master_key = "a".repeat(64);
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_buyer_pubkey)
            VALUES (?1, 'buy', 'ev1', 'active', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    1, 3, 0, 0, ?2)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(&master_key)
        .execute(&pool)
        .await
        .unwrap();
        let result = super::find_failed_payment_for_master_key(&pool, &master_key)
            .await
            .unwrap();
        assert!(
            result.is_empty(),
            "Should not return orders with wrong status"
        );
    }

    // -- Tests for find_order_by_hash --

    #[tokio::test]
    async fn test_find_order_by_hash_found() {
        let pool = setup_orders_db().await.unwrap();
        let id = uuid::Uuid::new_v4();
        let hash = "abc123def456";

        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid, hash)
            VALUES (?1, 'buy', 'ev1', 'active', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?2)"#,
        )
        .bind(id)
        .bind(hash)
        .execute(&pool)
        .await
        .unwrap();

        let result = super::find_order_by_hash(&pool, hash).await;
        assert!(result.is_ok(), "Should find order by hash");
    }

    #[tokio::test]
    async fn test_find_order_by_hash_not_found() {
        let pool = setup_orders_db().await.unwrap();

        let result = super::find_order_by_hash(&pool, "nonexistent_hash").await;
        assert!(result.is_err(), "Should error when hash not found");
    }

    // -- Tests for find_order_by_date --

    /// Phase 1.5 regression: `WaitingTakerBond` is a daemon-internal
    /// pre-trade status; on the wire it publishes as `pending`. The
    /// expiry job must cover both buckets — otherwise an order parked
    /// at `WaitingTakerBond` past its `expires_at` would never expire
    /// and its bond HTLCs would tie up taker funds in LND until CLTV.
    #[tokio::test]
    async fn test_find_order_by_date_includes_waiting_taker_bond() {
        let pool = setup_orders_db().await.unwrap();
        let now = nostr_sdk::Timestamp::now().as_secs() as i64;
        let past = now - 3600;
        let future = now + 3600;

        // Helper to insert an order with a specific status + expires_at.
        async fn insert(pool: &SqlitePool, id: uuid::Uuid, status: &str, expires_at: i64) {
            sqlx::query(
                r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid)
            VALUES (?1, 'buy', 'ev', ?2, 0, 'lightning',
                    1000, 'USD', 10, ?3, ?4,
                    0, 0, 0, 0)"#,
            )
            .bind(id)
            .bind(status)
            .bind(expires_at - 3600)
            .bind(expires_at)
            .execute(pool)
            .await
            .unwrap();
        }

        let pending_expired = uuid::Uuid::new_v4();
        let waiting_taker_bond_expired = uuid::Uuid::new_v4();
        let waiting_maker_bond_expired = uuid::Uuid::new_v4();
        let pending_fresh = uuid::Uuid::new_v4();
        let waiting_taker_bond_fresh = uuid::Uuid::new_v4();
        let waiting_maker_bond_fresh = uuid::Uuid::new_v4();
        let active_expired = uuid::Uuid::new_v4(); // out-of-bucket; must not match

        insert(&pool, pending_expired, "pending", past).await;
        insert(
            &pool,
            waiting_taker_bond_expired,
            "waiting-taker-bond",
            past,
        )
        .await;
        insert(
            &pool,
            waiting_maker_bond_expired,
            "waiting-maker-bond",
            past,
        )
        .await;
        insert(&pool, pending_fresh, "pending", future).await;
        insert(
            &pool,
            waiting_taker_bond_fresh,
            "waiting-taker-bond",
            future,
        )
        .await;
        insert(
            &pool,
            waiting_maker_bond_fresh,
            "waiting-maker-bond",
            future,
        )
        .await;
        insert(&pool, active_expired, "active", past).await;

        let expired = super::find_order_by_date(&pool).await.unwrap();
        let ids: std::collections::HashSet<uuid::Uuid> = expired.iter().map(|o| o.id).collect();

        assert!(
            ids.contains(&pending_expired),
            "expired Pending must be returned"
        );
        assert!(
            ids.contains(&waiting_taker_bond_expired),
            "expired WaitingTakerBond must be returned (Phase 1.5)"
        );
        assert!(
            ids.contains(&waiting_maker_bond_expired),
            "expired WaitingMakerBond must be returned (Phase 5)"
        );
        assert!(
            !ids.contains(&pending_fresh),
            "non-expired Pending must NOT be returned"
        );
        assert!(
            !ids.contains(&waiting_taker_bond_fresh),
            "non-expired WaitingTakerBond must NOT be returned"
        );
        assert!(
            !ids.contains(&waiting_maker_bond_fresh),
            "non-expired WaitingMakerBond must NOT be returned"
        );
        assert!(
            !ids.contains(&active_expired),
            "expired but non-pre-trade orders (e.g. active) must NOT be returned"
        );
    }

    // -- Tests for update_order_to_initial_state --

    #[tokio::test]
    async fn test_update_order_to_initial_state_resets_fields() {
        let pool = setup_orders_db().await.unwrap();
        let id = uuid::Uuid::new_v4();

        // Insert an active order with hash, preimage, invoice, etc.
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    hash, preimage, buyer_invoice, taken_at, invoice_held_at)
            VALUES (?1, 'buy', 'ev1', 'active', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 500, 0,
                    'somehash', 'somepreimage', 'someinvoice', 1700001000, 1700002000)"#,
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        let result = super::update_order_to_initial_state(&pool, id, 50000, 250, 100).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Should return true for existing order");

        // Verify status was reset
        let status: (String,) = sqlx::query_as("SELECT status FROM orders WHERE id = ?1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status.0, "pending", "Status should be reset to pending");

        // Verify amounts were updated
        let amounts: (i64, i64, i64) =
            sqlx::query_as("SELECT amount, fee, dev_fee FROM orders WHERE id = ?1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(amounts.0, 50000, "Amount should be updated");
        assert_eq!(amounts.1, 250, "Fee should be updated");
        assert_eq!(amounts.2, 100, "Dev fee should be updated");

        // Verify fields were cleared
        let cleared: (Option<String>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT hash, preimage, buyer_invoice FROM orders WHERE id = ?1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(cleared.0.is_none(), "Hash should be cleared");
        assert!(cleared.1.is_none(), "Preimage should be cleared");
        assert!(cleared.2.is_none(), "Buyer invoice should be cleared");

        // Verify timestamps were reset
        let times: (i64, i64) =
            sqlx::query_as("SELECT taken_at, invoice_held_at FROM orders WHERE id = ?1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(times.0, 0, "taken_at should be reset to 0");
        assert_eq!(times.1, 0, "invoice_held_at should be reset to 0");
    }

    #[tokio::test]
    async fn test_update_order_to_initial_state_nonexistent() {
        let pool = setup_orders_db().await.unwrap();
        let id = uuid::Uuid::new_v4();

        let result = super::update_order_to_initial_state(&pool, id, 50000, 250, 100).await;
        assert!(result.is_ok());
        assert!(
            !result.unwrap(),
            "Should return false for nonexistent order"
        );
    }

    // -- Tests for update_user_trade_index --

    async fn setup_users_db() -> Result<SqlitePool, Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(TEST_DB_URL)
            .await?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS users (
                pubkey char(64) primary key not null,
                is_admin integer not null default 0,
                admin_password char(64),
                is_solver integer not null default 0,
                is_banned integer not null default 0,
                category integer not null default 0,
                last_trade_index integer not null default 0,
                total_reviews integer not null default 0,
                total_rating real not null default 0.0,
                last_rating integer not null default 0,
                max_rating integer not null default 0,
                min_rating integer not null default 0,
                created_at integer not null
            )"#,
        )
        .execute(&pool)
        .await?;

        Ok(pool)
    }

    async fn insert_test_user(pool: &SqlitePool, pubkey: &str) {
        sqlx::query("INSERT INTO users (pubkey, created_at) VALUES (?1, 1700000000)")
            .bind(pubkey)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn setup_finalize_permission_db() -> Result<SqlitePool, Error> {
        let pool = setup_users_db().await?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS disputes (
                id char(36) primary key not null,
                order_id char(36) unique not null,
                status varchar(10) not null,
                order_previous_status varchar(10) not null,
                solver_pubkey char(64),
                created_at integer not null,
                taken_at integer default 0
            )"#,
        )
        .execute(&pool)
        .await?;
        Ok(pool)
    }

    async fn insert_assigned_dispute(pool: &SqlitePool, order_id: uuid::Uuid, solver_pubkey: &str) {
        sqlx::query(
            "INSERT INTO disputes (id, order_id, status, order_previous_status, solver_pubkey, created_at)
             VALUES (?1, ?2, 'in-progress', 'dispute', ?3, 1700000000)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(order_id)
        .bind(solver_pubkey)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_read_only_solver(pool: &SqlitePool, pubkey: &str) {
        sqlx::query(
            "INSERT INTO users (pubkey, is_solver, category, created_at) VALUES (?1, 1, 1, 1700000000)",
        )
        .bind(pubkey)
        .execute(pool)
        .await
        .unwrap();
    }

    const VALID_PUBKEY: &str = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
    const DAEMON_PUBKEY: &str = "b1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
    const HUMAN_SOLVER_PUBKEY: &str =
        "c1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";

    #[tokio::test]
    async fn ensure_dispute_finalize_permission_daemon_without_user_row() {
        let pool = setup_finalize_permission_db().await.unwrap();
        let order_id = uuid::Uuid::new_v4();
        insert_assigned_dispute(&pool, order_id, DAEMON_PUBKEY).await;

        let result = super::ensure_dispute_finalize_permission(
            &pool,
            DAEMON_PUBKEY,
            DAEMON_PUBKEY,
            order_id,
        )
        .await;

        assert!(
            result.is_ok(),
            "assigned daemon must finalize without a users row"
        );
    }

    #[tokio::test]
    async fn ensure_dispute_finalize_permission_human_solver_denied() {
        let pool = setup_finalize_permission_db().await.unwrap();
        let order_id = uuid::Uuid::new_v4();
        insert_read_only_solver(&pool, HUMAN_SOLVER_PUBKEY).await;
        insert_assigned_dispute(&pool, order_id, HUMAN_SOLVER_PUBKEY).await;

        let result = super::ensure_dispute_finalize_permission(
            &pool,
            HUMAN_SOLVER_PUBKEY,
            DAEMON_PUBKEY,
            order_id,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroError::MostroCantDo(CantDoReason::NotAuthorized))
        ));
    }

    #[tokio::test]
    async fn test_update_user_trade_index_valid() {
        let pool = setup_users_db().await.unwrap();
        insert_test_user(&pool, VALID_PUBKEY).await;

        let result = super::update_user_trade_index(&pool, VALID_PUBKEY.to_string(), 5).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Should return true for existing user");

        // Verify
        let idx: (i64,) = sqlx::query_as("SELECT last_trade_index FROM users WHERE pubkey = ?1")
            .bind(VALID_PUBKEY)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(idx.0, 5);
    }

    #[tokio::test]
    async fn test_update_user_trade_index_invalid_pubkey_short() {
        let pool = setup_users_db().await.unwrap();

        let result = super::update_user_trade_index(&pool, "abc123".to_string(), 5).await;
        assert!(result.is_err(), "Should reject short pubkey");
    }

    #[tokio::test]
    async fn test_update_user_trade_index_invalid_pubkey_non_hex() {
        let pool = setup_users_db().await.unwrap();

        let bad = "g1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let result = super::update_user_trade_index(&pool, bad.to_string(), 5).await;
        assert!(result.is_err(), "Should reject non-hex pubkey");
    }

    #[tokio::test]
    async fn test_update_user_trade_index_negative() {
        let pool = setup_users_db().await.unwrap();
        insert_test_user(&pool, VALID_PUBKEY).await;

        let result = super::update_user_trade_index(&pool, VALID_PUBKEY.to_string(), -1).await;
        assert!(result.is_err(), "Should reject negative trade index");
    }

    #[tokio::test]
    async fn test_update_user_trade_index_nonexistent_user() {
        let pool = setup_users_db().await.unwrap();

        let result = super::update_user_trade_index(&pool, VALID_PUBKEY.to_string(), 5).await;
        assert!(result.is_ok());
        assert!(!result.unwrap(), "Should return false for nonexistent user");
    }

    // -- Tests for buyer/seller_has_pending_order --

    #[tokio::test]
    async fn test_buyer_has_pending_order_true() {
        let pool = setup_orders_db().await.unwrap();

        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_buyer_pubkey)
            VALUES (?1, 'buy', 'ev1', 'waiting-buyer-invoice', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?2)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(VALID_PUBKEY)
        .execute(&pool)
        .await
        .unwrap();

        let result = super::buyer_has_pending_order(&pool, VALID_PUBKEY.to_string()).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Buyer should have pending order");
    }

    #[tokio::test]
    async fn test_buyer_has_pending_order_false() {
        let pool = setup_orders_db().await.unwrap();

        // Insert with different status
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_buyer_pubkey)
            VALUES (?1, 'buy', 'ev1', 'active', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?2)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(VALID_PUBKEY)
        .execute(&pool)
        .await
        .unwrap();

        let result = super::buyer_has_pending_order(&pool, VALID_PUBKEY.to_string()).await;
        assert!(result.is_ok());
        assert!(
            !result.unwrap(),
            "Buyer should NOT have pending order with wrong status"
        );
    }

    #[tokio::test]
    async fn test_seller_has_pending_order_true() {
        let pool = setup_orders_db().await.unwrap();

        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_seller_pubkey)
            VALUES (?1, 'sell', 'ev1', 'waiting-payment', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?2)"#,
        )
        .bind(uuid::Uuid::new_v4())
        .bind(VALID_PUBKEY)
        .execute(&pool)
        .await
        .unwrap();

        let result = super::seller_has_pending_order(&pool, VALID_PUBKEY.to_string()).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Seller should have pending order");
    }

    #[tokio::test]
    async fn test_has_pending_order_invalid_pubkey() {
        let pool = setup_orders_db().await.unwrap();

        let result = super::buyer_has_pending_order(&pool, "not_hex".to_string()).await;
        assert!(result.is_err(), "Should reject invalid pubkey");
    }

    // -- Tests for update_user_rating validation --

    #[tokio::test]
    async fn test_update_user_rating_valid() {
        let pool = setup_users_db().await.unwrap();
        insert_test_user(&pool, VALID_PUBKEY).await;

        let result =
            super::update_user_rating(&pool, VALID_PUBKEY.to_string(), 4, 3, 5, 10, 40.0).await;
        assert!(result.is_ok());
        assert!(result.unwrap(), "Should update existing user rating");

        // Verify
        let row: (i64, i64, i64, i64, f64) = sqlx::query_as(
            "SELECT last_rating, min_rating, max_rating, total_reviews, total_rating FROM users WHERE pubkey = ?1"
        )
        .bind(VALID_PUBKEY)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, 4);
        assert_eq!(row.1, 3);
        assert_eq!(row.2, 5);
        assert_eq!(row.3, 10);
        assert!((row.4 - 40.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_update_user_rating_invalid_pubkey() {
        let pool = setup_users_db().await.unwrap();

        let result = super::update_user_rating(&pool, "short".to_string(), 4, 3, 5, 10, 40.0).await;
        assert!(result.is_err(), "Should reject invalid pubkey");
    }

    #[tokio::test]
    async fn test_update_user_rating_out_of_range() {
        let pool = setup_users_db().await.unwrap();
        insert_test_user(&pool, VALID_PUBKEY).await;

        // Rating > 5
        let result =
            super::update_user_rating(&pool, VALID_PUBKEY.to_string(), 6, 3, 5, 10, 40.0).await;
        assert!(result.is_err(), "Should reject rating > 5");

        // Rating < 0
        let result =
            super::update_user_rating(&pool, VALID_PUBKEY.to_string(), -1, 3, 5, 10, 40.0).await;
        assert!(result.is_err(), "Should reject negative rating");
    }

    #[tokio::test]
    async fn test_update_user_rating_negative_reviews() {
        let pool = setup_users_db().await.unwrap();
        insert_test_user(&pool, VALID_PUBKEY).await;

        let result =
            super::update_user_rating(&pool, VALID_PUBKEY.to_string(), 4, 3, 5, -1, 40.0).await;
        assert!(result.is_err(), "Should reject negative total_reviews");
    }

    #[tokio::test]
    async fn test_update_user_rating_total_exceeds_max() {
        let pool = setup_users_db().await.unwrap();
        insert_test_user(&pool, VALID_PUBKEY).await;

        // total_rating > total_reviews * 5
        let result =
            super::update_user_rating(&pool, VALID_PUBKEY.to_string(), 4, 3, 5, 2, 11.0).await;
        assert!(result.is_err(), "Should reject total_rating > reviews * 5");
    }

    #[tokio::test]
    async fn test_update_user_rating_min_gt_last() {
        let pool = setup_users_db().await.unwrap();
        insert_test_user(&pool, VALID_PUBKEY).await;

        // min_rating > last_rating
        let result =
            super::update_user_rating(&pool, VALID_PUBKEY.to_string(), 2, 3, 5, 10, 40.0).await;
        assert!(result.is_err(), "Should reject min_rating > last_rating");
    }

    #[tokio::test]
    async fn test_update_user_rating_last_gt_max() {
        let pool = setup_users_db().await.unwrap();
        insert_test_user(&pool, VALID_PUBKEY).await;

        // last_rating > max_rating
        let result =
            super::update_user_rating(&pool, VALID_PUBKEY.to_string(), 5, 3, 4, 10, 40.0).await;
        assert!(result.is_err(), "Should reject last_rating > max_rating");
    }

    // -- Tests for reset_order_taken_at_time --

    #[tokio::test]
    async fn test_reset_order_taken_at_time() {
        let pool = setup_orders_db().await.unwrap();
        let id = uuid::Uuid::new_v4();

        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    taken_at)
            VALUES (?1, 'buy', 'ev1', 'active', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, 1700005000)"#,
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        let result = super::reset_order_taken_at_time(&pool, id).await;
        assert!(result.is_ok());

        let row: (i64,) = sqlx::query_as("SELECT taken_at FROM orders WHERE id = ?1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 0, "taken_at should be reset to 0");
    }

    // -- Tests for update_order_invoice_held_at_time --

    #[tokio::test]
    async fn test_update_order_invoice_held_at_time() {
        let pool = setup_orders_db().await.unwrap();
        let id = uuid::Uuid::new_v4();

        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    invoice_held_at)
            VALUES (?1, 'buy', 'ev1', 'active', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, 0)"#,
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        let result = super::update_order_invoice_held_at_time(&pool, id, 1700005000).await;
        assert!(result.is_ok());

        let row: (i64,) = sqlx::query_as("SELECT invoice_held_at FROM orders WHERE id = ?1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.0, 1700005000,
            "invoice_held_at should be set to provided value"
        );
    }

    // -- Smoke tests for the runtime UPDATE helpers (issue #792) --
    //
    // The five helpers below were migrated off compile-time-checked
    // `sqlx::query!` in the sqlx 0.9 upgrade. The tests above already
    // assert that the target columns change; these add the other half of
    // the safety net: sentinel-valued rows proving that untouched columns
    // keep their values and that the WHERE clause never leaks onto other
    // rows.

    /// Insert an order whose non-target columns carry `tag`-derived
    /// sentinels, so any accidental overwrite is detectable.
    async fn insert_sentinel_order(pool: &SqlitePool, id: uuid::Uuid, tag: &str) {
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    hash, preimage, buyer_invoice, taken_at, invoice_held_at,
                    fee, routing_fee)
            VALUES (?1, 'sell', ?2, 'active', 7, 'face to face',
                    111111, 'EUR', 42, 1700000001, 1700086401,
                    0, 0, 33, 0,
                    ?3, ?4, ?5, 1700001111, 1700002222,
                    21, 9)"#,
        )
        .bind(id)
        .bind(format!("ev-{tag}"))
        .bind(format!("hash-{tag}"))
        .bind(format!("preimage-{tag}"))
        .bind(format!("invoice-{tag}"))
        .execute(pool)
        .await
        .unwrap();
    }

    /// Insert a user whose non-target columns carry non-default sentinels.
    async fn insert_sentinel_user(pool: &SqlitePool, pubkey: &str) {
        sqlx::query(
            r#"INSERT INTO users (pubkey, is_admin, is_solver, is_banned, category,
                    last_trade_index, total_reviews, total_rating, last_rating,
                    max_rating, min_rating, created_at)
            VALUES (?1, 1, 1, 0, 2, 77, 8, 32.0, 4, 5, 2, 1700000123)"#,
        )
        .bind(pubkey)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn update_order_to_initial_state_leaves_untouched_columns_and_other_rows() {
        let pool = setup_orders_db().await.unwrap();
        let target = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        insert_sentinel_order(&pool, target, "a").await;
        insert_sentinel_order(&pool, other, "b").await;

        assert!(
            super::update_order_to_initial_state(&pool, target, 50000, 250, 100)
                .await
                .unwrap()
        );

        // Target row: columns outside the UPDATE's SET list keep sentinels.
        let untouched: (String, String, i64, String, String, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT kind, event_id, premium, payment_method, fiat_code,
                    fiat_amount, created_at, expires_at, routing_fee
             FROM orders WHERE id = ?1",
        )
        .bind(target)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(untouched.0, "sell", "kind must not change");
        assert_eq!(untouched.1, "ev-a", "event_id must not change");
        assert_eq!(untouched.2, 7, "premium must not change");
        assert_eq!(
            untouched.3, "face to face",
            "payment_method must not change"
        );
        assert_eq!(untouched.4, "EUR", "fiat_code must not change");
        assert_eq!(untouched.5, 42, "fiat_amount must not change");
        assert_eq!(untouched.6, 1700000001, "created_at must not change");
        assert_eq!(untouched.7, 1700086401, "expires_at must not change");
        assert_eq!(untouched.8, 9, "routing_fee must not change");

        // Other row: the columns the helper writes stay untouched (WHERE
        // must pin the update to the target id).
        let other_row: (String, i64, Option<String>, i64) =
            sqlx::query_as("SELECT status, amount, hash, taken_at FROM orders WHERE id = ?1")
                .bind(other)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(other_row.0, "active", "other row's status must not change");
        assert_eq!(other_row.1, 111111, "other row's amount must not change");
        assert_eq!(
            other_row.2.as_deref(),
            Some("hash-b"),
            "other row's hash must not be cleared"
        );
        assert_eq!(
            other_row.3, 1700001111,
            "other row's taken_at must not be reset"
        );
    }

    #[tokio::test]
    async fn reset_order_taken_at_time_only_touches_taken_at_of_target_row() {
        let pool = setup_orders_db().await.unwrap();
        let target = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        insert_sentinel_order(&pool, target, "a").await;
        insert_sentinel_order(&pool, other, "b").await;

        assert!(super::reset_order_taken_at_time(&pool, target)
            .await
            .unwrap());

        let row: (i64, String, i64, i64, Option<String>) = sqlx::query_as(
            "SELECT taken_at, status, amount, invoice_held_at, hash FROM orders WHERE id = ?1",
        )
        .bind(target)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, 0, "taken_at must be reset");
        assert_eq!(row.1, "active", "status must not change");
        assert_eq!(row.2, 111111, "amount must not change");
        assert_eq!(row.3, 1700002222, "invoice_held_at must not change");
        assert_eq!(row.4.as_deref(), Some("hash-a"), "hash must not change");

        let other_taken_at: (i64,) = sqlx::query_as("SELECT taken_at FROM orders WHERE id = ?1")
            .bind(other)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            other_taken_at.0, 1700001111,
            "other row's taken_at must not be reset"
        );
    }

    #[tokio::test]
    async fn update_order_invoice_held_at_time_only_touches_target_row() {
        let pool = setup_orders_db().await.unwrap();
        let target = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        insert_sentinel_order(&pool, target, "a").await;
        insert_sentinel_order(&pool, other, "b").await;

        assert!(
            super::update_order_invoice_held_at_time(&pool, target, 1700009999)
                .await
                .unwrap()
        );

        let row: (i64, String, i64, i64) = sqlx::query_as(
            "SELECT invoice_held_at, status, amount, taken_at FROM orders WHERE id = ?1",
        )
        .bind(target)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, 1700009999, "invoice_held_at must be set");
        assert_eq!(row.1, "active", "status must not change");
        assert_eq!(row.2, 111111, "amount must not change");
        assert_eq!(row.3, 1700001111, "taken_at must not change");

        let other_held_at: (i64,) =
            sqlx::query_as("SELECT invoice_held_at FROM orders WHERE id = ?1")
                .bind(other)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            other_held_at.0, 1700002222,
            "other row's invoice_held_at must not change"
        );
    }

    #[tokio::test]
    async fn update_user_trade_index_only_touches_last_trade_index_of_target_user() {
        let pool = setup_users_db().await.unwrap();
        insert_sentinel_user(&pool, VALID_PUBKEY).await;
        insert_sentinel_user(&pool, DAEMON_PUBKEY).await;

        assert!(
            super::update_user_trade_index(&pool, VALID_PUBKEY.to_string(), 100)
                .await
                .unwrap()
        );

        let row: (i64, i64, i64, i64, i64, f64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT last_trade_index, is_admin, is_solver, category, total_reviews,
                    total_rating, last_rating, max_rating, min_rating, created_at
             FROM users WHERE pubkey = ?1",
        )
        .bind(VALID_PUBKEY)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, 100, "last_trade_index must be updated");
        assert_eq!(row.1, 1, "is_admin must not change");
        assert_eq!(row.2, 1, "is_solver must not change");
        assert_eq!(row.3, 2, "category must not change");
        assert_eq!(row.4, 8, "total_reviews must not change");
        assert!(
            (row.5 - 32.0).abs() < f64::EPSILON,
            "total_rating must not change"
        );
        assert_eq!(row.6, 4, "last_rating must not change");
        assert_eq!(row.7, 5, "max_rating must not change");
        assert_eq!(row.8, 2, "min_rating must not change");
        assert_eq!(row.9, 1700000123, "created_at must not change");

        let other_idx: (i64,) =
            sqlx::query_as("SELECT last_trade_index FROM users WHERE pubkey = ?1")
                .bind(DAEMON_PUBKEY)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            other_idx.0, 77,
            "other user's last_trade_index must not change"
        );
    }

    #[tokio::test]
    async fn update_user_rating_only_touches_rating_columns_of_target_user() {
        let pool = setup_users_db().await.unwrap();
        insert_sentinel_user(&pool, VALID_PUBKEY).await;
        insert_sentinel_user(&pool, DAEMON_PUBKEY).await;

        assert!(
            super::update_user_rating(&pool, VALID_PUBKEY.to_string(), 4, 3, 5, 10, 40.0)
                .await
                .unwrap()
        );

        let row: (i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT is_admin, is_solver, category, last_trade_index, created_at
             FROM users WHERE pubkey = ?1",
        )
        .bind(VALID_PUBKEY)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, 1, "is_admin must not change");
        assert_eq!(row.1, 1, "is_solver must not change");
        assert_eq!(row.2, 2, "category must not change");
        assert_eq!(row.3, 77, "last_trade_index must not change");
        assert_eq!(row.4, 1700000123, "created_at must not change");

        let other: (i64, i64, i64, i64, f64) = sqlx::query_as(
            "SELECT last_rating, min_rating, max_rating, total_reviews, total_rating
             FROM users WHERE pubkey = ?1",
        )
        .bind(DAEMON_PUBKEY)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(other.0, 4, "other user's last_rating must not change");
        assert_eq!(other.1, 2, "other user's min_rating must not change");
        assert_eq!(other.2, 5, "other user's max_rating must not change");
        assert_eq!(other.3, 8, "other user's total_reviews must not change");
        assert!(
            (other.4 - 32.0).abs() < f64::EPSILON,
            "other user's total_rating must not change"
        );
    }

    // -- Tests for the CF-4 cashu escrow helpers --

    use mostro_core::order::Status;

    async fn insert_cashu_test_order(pool: &SqlitePool, id: uuid::Uuid, status: &str) {
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid)
            VALUES (?1, 'sell', ?2, ?3, 0, 'face to face',
                    100000, 'EUR', 100, 1700000000, 1700086400,
                    0, 0, 0, 0)"#,
        )
        .bind(id)
        .bind(format!("ev-{id}"))
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn cashu_columns(
        pool: &SqlitePool,
        id: uuid::Uuid,
    ) -> (Option<String>, Option<String>, Option<i64>, String) {
        sqlx::query_as(
            "SELECT cashu_mint_url, cashu_escrow_token, cashu_escrow_locked_at, status
             FROM orders WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn cashu_escrow_cas_locks_once_and_replay_is_noop() {
        let pool = setup_orders_db().await.unwrap();
        let id = uuid::Uuid::new_v4();
        insert_cashu_test_order(&pool, id, &Status::WaitingPayment.to_string()).await;

        // First submission: persists the escrow and advances the status in
        // one write.
        let locked = super::update_order_cashu_escrow(
            &pool,
            id,
            "https://mint.example.com",
            "cashuAtoken",
            1700000100,
            Status::WaitingPayment,
            Status::Active,
        )
        .await
        .unwrap();
        assert!(locked, "first CAS must match the row");

        let (mint, token, locked_at, status) = cashu_columns(&pool, id).await;
        assert_eq!(mint.as_deref(), Some("https://mint.example.com"));
        assert_eq!(token.as_deref(), Some("cashuAtoken"));
        assert_eq!(locked_at, Some(1700000100));
        assert_eq!(status, Status::Active.to_string());

        // Replay: cashu_escrow_locked_at is no longer NULL (and the status
        // moved on), so the CAS matches zero rows and nothing is rewritten.
        let replayed = super::update_order_cashu_escrow(
            &pool,
            id,
            "https://evil.example.com",
            "cashuAother",
            1700009999,
            Status::WaitingPayment,
            Status::Active,
        )
        .await
        .unwrap();
        assert!(!replayed, "replayed CAS must match zero rows");

        let (mint, token, locked_at, _) = cashu_columns(&pool, id).await;
        assert_eq!(mint.as_deref(), Some("https://mint.example.com"));
        assert_eq!(token.as_deref(), Some("cashuAtoken"));
        assert_eq!(locked_at, Some(1700000100), "original lock must survive");
    }

    #[tokio::test]
    async fn cashu_escrow_cas_status_mismatch_matches_zero_rows() {
        let pool = setup_orders_db().await.unwrap();
        let id = uuid::Uuid::new_v4();
        insert_cashu_test_order(&pool, id, &Status::Pending.to_string()).await;

        let locked = super::update_order_cashu_escrow(
            &pool,
            id,
            "https://mint.example.com",
            "cashuAtoken",
            1700000100,
            Status::WaitingPayment,
            Status::Active,
        )
        .await
        .unwrap();
        assert!(!locked, "status mismatch must match zero rows");

        let (mint, token, locked_at, status) = cashu_columns(&pool, id).await;
        assert!(mint.is_none(), "escrow columns must stay untouched");
        assert!(token.is_none());
        assert!(locked_at.is_none());
        assert_eq!(status, Status::Pending.to_string());
    }

    #[tokio::test]
    async fn find_locked_cashu_orders_includes_locked_regardless_of_status() {
        let pool = setup_orders_db().await.unwrap();
        let locked_id = uuid::Uuid::new_v4();
        let unlocked_id = uuid::Uuid::new_v4();
        insert_cashu_test_order(&pool, locked_id, &Status::WaitingPayment.to_string()).await;
        insert_cashu_test_order(&pool, unlocked_id, &Status::Active.to_string()).await;

        assert!(super::update_order_cashu_escrow(
            &pool,
            locked_id,
            "https://mint.example.com",
            "cashuAtoken",
            1700000100,
            Status::WaitingPayment,
            Status::Active,
        )
        .await
        .unwrap());

        // Move the locked order past Active: the finder must still return
        // it (CF-4: no status predicate — the escrow is what matters).
        sqlx::query("UPDATE orders SET status = ?1 WHERE id = ?2")
            .bind(Status::FiatSent.to_string())
            .bind(locked_id)
            .execute(&pool)
            .await
            .unwrap();

        let locked = super::find_locked_cashu_orders(&pool).await.unwrap();
        assert_eq!(locked.len(), 1, "only the locked order is returned");
        assert_eq!(locked[0].id, locked_id);
        assert_eq!(
            locked[0].status,
            Status::FiatSent.to_string(),
            "a locked order past Active must still be found"
        );
    }

    #[tokio::test]
    async fn cashu_escrow_cas_on_missing_order_matches_zero_rows() {
        // A CAS against an id that isn't in the table must report no match
        // (not error), the same "matches zero rows" contract a replay hits.
        let pool = setup_orders_db().await.unwrap();
        let locked = super::update_order_cashu_escrow(
            &pool,
            uuid::Uuid::new_v4(),
            "https://mint.example.com",
            "cashuAtoken",
            1700000100,
            Status::WaitingPayment,
            Status::Active,
        )
        .await
        .unwrap();
        assert!(!locked, "CAS on a non-existent order must match zero rows");
    }

    #[tokio::test]
    async fn find_locked_cashu_orders_includes_terminal_status_orders() {
        // Pins the deliberate CF-4 design (M-1): the finder has no status
        // predicate and nothing ever clears `cashu_escrow_locked_at`, so a
        // locked order that reached a terminal status is STILL returned.
        // This is a conscious, tested contract — if CF-5 needs terminal
        // orders excluded, it must clear the lock or filter, not rely on
        // this finder to drop them.
        let pool = setup_orders_db().await.unwrap();
        let id = uuid::Uuid::new_v4();
        insert_cashu_test_order(&pool, id, &Status::WaitingPayment.to_string()).await;

        assert!(super::update_order_cashu_escrow(
            &pool,
            id,
            "https://mint.example.com",
            "cashuAtoken",
            1700000100,
            Status::WaitingPayment,
            Status::Active,
        )
        .await
        .unwrap());

        sqlx::query("UPDATE orders SET status = ?1 WHERE id = ?2")
            .bind(Status::Canceled.to_string())
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let locked = super::find_locked_cashu_orders(&pool).await.unwrap();
        assert_eq!(
            locked.len(),
            1,
            "a canceled-but-locked order is still found"
        );
        assert_eq!(locked[0].id, id);
        assert_eq!(locked[0].status, Status::Canceled.to_string());
    }
}

// Coverage-focused tests running against the real migration set, so schema
// helpers, admin/dispute permission checks and the restore-session pipeline
// are exercised on the production schema.
#[cfg(test)]
mod migration_and_query_tests {
    use super::*;
    use crate::app::context::test_utils::test_settings;
    use crate::config::MOSTRO_CONFIG;
    use mostro_core::prelude::CantDoReason;
    use sqlx::sqlite::SqlitePoolOptions;

    fn init_test_settings() {
        let _ = MOSTRO_CONFIG.set(test_settings());
    }

    /// Migrated in-memory pool pinned to **one** connection.
    ///
    /// `sqlite::memory:` gives every *connection* its own private database,
    /// so a multi-connection pool can hand a second caller a blank schema.
    /// `restore_session_manager_delivers_background_results` clones this pool
    /// into a `spawn_blocking` worker: on a different connection that worker
    /// would find neither the migrations nor the inserted order, log an
    /// error, deliver nothing, and leave the untimed `wait_for_result()`
    /// blocked forever. Capping at one connection keeps every acquisition on
    /// the same database.
    async fn migrated_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    const HEX_KEY_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEX_KEY_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[allow(clippy::too_many_arguments)]
    async fn insert_order(
        pool: &SqlitePool,
        id: Uuid,
        kind: &str,
        status: &str,
        buyer: Option<&str>,
        seller: Option<&str>,
        creator: &str,
        taken_at: i64,
    ) {
        sqlx::query(
            r#"INSERT INTO orders (
                id, kind, event_id, creator_pubkey, buyer_pubkey, master_buyer_pubkey,
                seller_pubkey, master_seller_pubkey, status, payment_method, amount,
                fiat_code, fiat_amount, premium, taken_at, created_at, expires_at,
                trade_index_buyer, trade_index_seller
            ) VALUES (?1, ?2, 'ev', ?3, ?4, ?5, ?6, ?7, ?8, 'bank', 1000, 'USD', 10, 0,
                      ?9, 0, 0, 7, 9)"#,
        )
        .bind(id)
        .bind(kind)
        .bind(creator)
        .bind(buyer)
        .bind(buyer)
        .bind(seller)
        .bind(seller)
        .bind(status)
        .bind(taken_at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_dispute(pool: &SqlitePool, order_id: Uuid, status: &str, solver: Option<&str>) {
        sqlx::query(
            r#"INSERT INTO disputes (id, order_id, status, order_previous_status, solver_pubkey, created_at, taken_at)
               VALUES (?1, ?2, ?3, 'active', ?4, 0, 0)"#,
        )
        .bind(Uuid::new_v4())
        .bind(order_id)
        .bind(status)
        .bind(solver)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_user(
        pool: &SqlitePool,
        pubkey: &str,
        is_admin: bool,
        admin_password: Option<&str>,
        is_solver: bool,
        category: i64,
    ) {
        sqlx::query(
            r#"INSERT INTO users (pubkey, is_admin, admin_password, is_solver, is_banned,
               category, last_trade_index, total_reviews, total_rating, last_rating,
               max_rating, min_rating, created_at)
               VALUES (?1, ?2, ?3, ?4, 0, ?5, 0, 0, 0, 0, 0, 0, 0)"#,
        )
        .bind(pubkey)
        .bind(is_admin)
        .bind(admin_password)
        .bind(is_solver)
        .bind(category)
        .execute(pool)
        .await
        .unwrap();
    }

    // ── schema helpers ───────────────────────────────────────────────────

    #[tokio::test]
    async fn table_column_exists_detects_present_and_absent_columns() {
        let pool = migrated_pool().await;
        assert!(table_column_exists(&pool, "orders", "dev_fee")
            .await
            .unwrap());
        assert!(!table_column_exists(&pool, "orders", "no_such_column")
            .await
            .unwrap());
        assert!(!table_column_exists(&pool, "no_such_table", "x")
            .await
            .unwrap());
    }

    #[test]
    fn strip_sql_comments_removes_only_comment_lines() {
        let sql = "-- header comment\nALTER TABLE t ADD COLUMN c INTEGER;\n  -- indented comment\nSELECT 1;";
        let stripped = strip_sql_comments(sql);
        assert!(!stripped.contains("comment"));
        assert!(stripped.contains("ALTER TABLE t ADD COLUMN c INTEGER;"));
        assert!(stripped.contains("SELECT 1;"));
    }

    #[test]
    fn normalize_sql_identifier_strips_quotes_and_commas() {
        assert_eq!(normalize_sql_identifier(" \"orders\","), "orders");
        assert_eq!(normalize_sql_identifier("`bonds`"), "bonds");
        assert_eq!(normalize_sql_identifier("[users]"), "users");
        assert_eq!(normalize_sql_identifier("plain"), "plain");
    }

    #[test]
    fn parse_add_column_statements_accepts_only_pure_add_column_migrations() {
        let ops = parse_add_column_statements(
            "-- adds two columns\nALTER TABLE orders ADD COLUMN dev_fee INTEGER DEFAULT 0;\nALTER TABLE \"orders\" ADD COLUMN dev_fee_paid INTEGER NOT NULL DEFAULT 0;",
        )
        .expect("pure add-column migration parses");
        assert_eq!(
            ops,
            vec![
                ("orders".to_string(), "dev_fee".to_string()),
                ("orders".to_string(), "dev_fee_paid".to_string()),
            ]
        );

        // Mixed statement kinds: not a pure add-column migration.
        assert!(parse_add_column_statements(
            "ALTER TABLE orders ADD COLUMN a INTEGER; CREATE INDEX i ON orders(a);"
        )
        .is_none());
        // Too few tokens.
        assert!(parse_add_column_statements("ALTER TABLE orders ADD COLUMN").is_none());
        // Empty input.
        assert!(parse_add_column_statements("-- only comments\n").is_none());
    }

    #[tokio::test]
    async fn applied_migration_versions_lists_all_applied_migrations() {
        let pool = migrated_pool().await;
        let versions = applied_migration_versions(&pool).await.unwrap();
        assert!(!versions.is_empty());
        let mut sorted = versions.clone();
        sorted.sort();
        assert_eq!(versions, sorted, "versions must come back ordered");
    }

    /// Replaying an add-column migration on a database that already has the
    /// columns produces the exact "duplicate column name" error `connect`
    /// reconciles; pin the parser on the real error object.
    #[tokio::test]
    async fn parse_duplicate_column_name_extracts_column_from_real_migrate_error() {
        let pool = migrated_pool().await;
        // Forget the dev_fee add-column migration was applied.
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 20251126120000")
            .execute(&pool)
            .await
            .unwrap();
        let err = sqlx::migrate!()
            .run(&pool)
            .await
            .expect_err("replaying the add-column migration must fail");
        let column = parse_duplicate_column_name(&err).expect("duplicate column error parses");
        assert_eq!(column, "dev_fee");
    }

    #[tokio::test]
    async fn reconcile_add_column_migration_records_and_unblocks_migrator() {
        let pool = migrated_pool().await;
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 20251126120000")
            .execute(&pool)
            .await
            .unwrap();

        let migrator = sqlx::migrate!();
        let reconciled = reconcile_existing_add_column_migration(&pool, &migrator, "dev_fee")
            .await
            .unwrap();
        assert!(reconciled, "existing columns must be recorded as applied");

        // The migrator must now run cleanly again.
        migrator.run(&pool).await.expect("migrations run clean");

        // A column no pending migration adds is not reconcilable.
        let not_reconciled =
            reconcile_existing_add_column_migration(&pool, &migrator, "no_such_column")
                .await
                .unwrap();
        assert!(!not_reconciled);
    }

    // ── legacy disputes-table token-column migration ─────────────────────

    #[tokio::test]
    async fn migrate_remove_token_columns_is_noop_without_token_columns() {
        let pool = migrated_pool().await;
        migrate_remove_token_columns(&pool).await.unwrap();
        assert!(!table_column_exists(&pool, "disputes", "buyer_token")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn migrate_remove_token_columns_drops_legacy_columns_and_keeps_rows() {
        init_test_settings();
        let pool = migrated_pool().await;
        // Recreate the legacy shape.
        sqlx::query("ALTER TABLE disputes ADD COLUMN buyer_token INTEGER")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE disputes ADD COLUMN seller_token INTEGER")
            .execute(&pool)
            .await
            .unwrap();
        let order_id = Uuid::new_v4();
        insert_dispute(&pool, order_id, "initiated", Some(HEX_KEY_A)).await;

        migrate_remove_token_columns(&pool).await.unwrap();

        assert!(!table_column_exists(&pool, "disputes", "buyer_token")
            .await
            .unwrap());
        assert!(!table_column_exists(&pool, "disputes", "seller_token")
            .await
            .unwrap());
        let dispute = find_dispute_by_order_id(&pool, order_id).await.unwrap();
        assert_eq!(dispute.order_id, order_id);
    }

    #[tokio::test]
    async fn migrate_remove_token_columns_handles_single_legacy_column() {
        let pool = migrated_pool().await;
        sqlx::query("ALTER TABLE disputes ADD COLUMN seller_token INTEGER")
            .execute(&pool)
            .await
            .unwrap();
        migrate_remove_token_columns(&pool).await.unwrap();
        assert!(!table_column_exists(&pool, "disputes", "seller_token")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn rebuild_disputes_table_preserves_rows() {
        let pool = migrated_pool().await;
        let order_id = Uuid::new_v4();
        insert_dispute(&pool, order_id, "in-progress", Some(HEX_KEY_B)).await;

        rebuild_disputes_table_without_tokens(&pool).await.unwrap();

        let dispute = find_dispute_by_order_id(&pool, order_id).await.unwrap();
        assert_eq!(dispute.order_id, order_id);
        assert_eq!(dispute.status, "in-progress");
    }

    // ── admin / permission queries ───────────────────────────────────────

    #[tokio::test]
    async fn get_admin_password_returns_none_without_admin_row() {
        let pool = migrated_pool().await;
        assert_eq!(get_admin_password(&pool).await.unwrap(), None);
    }

    #[tokio::test]
    async fn get_admin_password_returns_stored_hash() {
        let pool = migrated_pool().await;
        insert_user(&pool, HEX_KEY_A, true, Some("argon2-hash"), false, 0).await;
        assert_eq!(
            get_admin_password(&pool).await.unwrap(),
            Some("argon2-hash".to_string())
        );
    }

    #[tokio::test]
    async fn ensure_finalize_permission_rejects_unassigned_caller() {
        let pool = migrated_pool().await;
        let order_id = Uuid::new_v4();
        insert_dispute(&pool, order_id, "in-progress", Some(HEX_KEY_A)).await;
        let err = ensure_dispute_finalize_permission(&pool, HEX_KEY_B, HEX_KEY_A, order_id)
            .await
            .expect_err("unassigned caller must be rejected");
        assert!(matches!(
            err,
            MostroError::MostroCantDo(CantDoReason::IsNotYourDispute)
        ));
    }

    #[tokio::test]
    async fn ensure_finalize_permission_allows_read_write_solver() {
        let pool = migrated_pool().await;
        let order_id = Uuid::new_v4();
        insert_dispute(&pool, order_id, "in-progress", Some(HEX_KEY_A)).await;
        insert_user(&pool, HEX_KEY_A, false, None, true, 2).await;
        // Caller is the assigned solver with category 2 (read-write), and is
        // NOT the daemon key — the solver_has_write_permission branch runs.
        ensure_dispute_finalize_permission(&pool, HEX_KEY_A, HEX_KEY_B, order_id)
            .await
            .expect("read-write assigned solver may finalize");
    }

    #[tokio::test]
    async fn user_has_solver_write_permission_requires_category_two() {
        let pool = migrated_pool().await;
        insert_user(&pool, HEX_KEY_A, false, None, true, 2).await;
        insert_user(&pool, HEX_KEY_B, false, None, true, 1).await;
        assert!(user_has_solver_write_permission(&pool, HEX_KEY_A)
            .await
            .unwrap());
        assert!(!user_has_solver_write_permission(&pool, HEX_KEY_B)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn is_dispute_taken_by_admin_distinguishes_solver_and_admin() {
        let pool = migrated_pool().await;
        let admin = HEX_KEY_A;

        // No dispute at all.
        assert!(!is_dispute_taken_by_admin(&pool, Uuid::new_v4(), admin)
            .await
            .unwrap());

        // In-progress dispute taken by the admin key.
        let admin_order = Uuid::new_v4();
        insert_dispute(&pool, admin_order, "in-progress", Some(admin)).await;
        assert!(is_dispute_taken_by_admin(&pool, admin_order, admin)
            .await
            .unwrap());

        // In-progress dispute taken by a human solver.
        let human_order = Uuid::new_v4();
        insert_dispute(&pool, human_order, "in-progress", Some(HEX_KEY_B)).await;
        assert!(!is_dispute_taken_by_admin(&pool, human_order, admin)
            .await
            .unwrap());

        // In-progress dispute with no solver assigned.
        let orphan_order = Uuid::new_v4();
        insert_dispute(&pool, orphan_order, "in-progress", None).await;
        assert!(!is_dispute_taken_by_admin(&pool, orphan_order, admin)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn find_solver_pubkey_returns_solver_row() {
        let pool = migrated_pool().await;
        insert_user(&pool, HEX_KEY_A, false, None, true, 2).await;
        let user = find_solver_pubkey(&pool, HEX_KEY_A.to_string())
            .await
            .unwrap();
        assert_eq!(user.pubkey, HEX_KEY_A);
        assert!(find_solver_pubkey(&pool, HEX_KEY_B.to_string())
            .await
            .is_err());
    }

    // ── order queries ────────────────────────────────────────────────────

    #[tokio::test]
    async fn edit_pubkeys_order_clears_counterparty_keys_by_kind() {
        let pool = migrated_pool().await;

        // Buy order: seller side must be cleared.
        let buy_id = Uuid::new_v4();
        insert_order(
            &pool,
            buy_id,
            "buy",
            "pending",
            Some(HEX_KEY_A),
            Some(HEX_KEY_B),
            HEX_KEY_A,
            0,
        )
        .await;
        let buy_order = sqlx::query_as::<_, Order>("SELECT * FROM orders WHERE id = ?1")
            .bind(buy_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let edited = edit_pubkeys_order(&pool, &buy_order).await.unwrap();
        assert_eq!(edited.seller_pubkey, None);
        assert_eq!(edited.master_seller_pubkey, None);
        assert_eq!(edited.buyer_pubkey.as_deref(), Some(HEX_KEY_A));

        // Sell order: buyer side must be cleared.
        let sell_id = Uuid::new_v4();
        insert_order(
            &pool,
            sell_id,
            "sell",
            "pending",
            Some(HEX_KEY_A),
            Some(HEX_KEY_B),
            HEX_KEY_B,
            0,
        )
        .await;
        let sell_order = sqlx::query_as::<_, Order>("SELECT * FROM orders WHERE id = ?1")
            .bind(sell_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let edited = edit_pubkeys_order(&pool, &sell_order).await.unwrap();
        assert_eq!(edited.buyer_pubkey, None);
        assert_eq!(edited.master_buyer_pubkey, None);
        assert_eq!(edited.seller_pubkey.as_deref(), Some(HEX_KEY_B));
    }

    #[tokio::test]
    async fn edit_pubkeys_order_rejects_invalid_kind_and_missing_row() {
        let pool = migrated_pool().await;

        // Unknown kind string.
        let bogus = Order {
            id: Uuid::new_v4(),
            kind: "swap".to_string(),
            ..Default::default()
        };
        assert!(edit_pubkeys_order(&pool, &bogus).await.is_err());

        // Valid kind but no matching row.
        let missing = Order {
            id: Uuid::new_v4(),
            kind: "sell".to_string(),
            ..Default::default()
        };
        assert!(edit_pubkeys_order(&pool, &missing).await.is_err());
    }

    #[tokio::test]
    async fn find_order_by_seconds_returns_only_stale_waiting_orders() {
        init_test_settings();
        let pool = migrated_pool().await;

        // Stale waiting-buyer-invoice: eligible.
        let stale_id = Uuid::new_v4();
        insert_order(
            &pool,
            stale_id,
            "sell",
            "waiting-buyer-invoice",
            Some(HEX_KEY_A),
            Some(HEX_KEY_B),
            HEX_KEY_B,
            1, // taken long ago
        )
        .await;
        // Fresh waiting-payment: not yet eligible.
        insert_order(
            &pool,
            Uuid::new_v4(),
            "buy",
            "waiting-payment",
            Some(HEX_KEY_A),
            Some(HEX_KEY_B),
            HEX_KEY_A,
            Timestamp::now().as_secs() as i64 + 10_000,
        )
        .await;
        // Stale but active: wrong status.
        insert_order(
            &pool,
            Uuid::new_v4(),
            "sell",
            "active",
            Some(HEX_KEY_A),
            Some(HEX_KEY_B),
            HEX_KEY_B,
            1,
        )
        .await;

        let stale = find_order_by_seconds(&pool).await.unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].id, stale_id);
    }

    #[tokio::test]
    async fn find_dispute_by_order_id_finds_and_misses() {
        let pool = migrated_pool().await;
        let order_id = Uuid::new_v4();
        insert_dispute(&pool, order_id, "initiated", None).await;
        assert_eq!(
            find_dispute_by_order_id(&pool, order_id)
                .await
                .unwrap()
                .order_id,
            order_id
        );
        assert!(find_dispute_by_order_id(&pool, Uuid::new_v4())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn has_pending_order_rejects_unknown_master_key_field() {
        let pool = migrated_pool().await;
        let err = has_pending_order_with_status(
            &pool,
            HEX_KEY_A.to_string(),
            "not_a_key_field",
            "waiting-payment",
        )
        .await
        .expect_err("unknown master key field must be rejected");
        assert!(err.to_string().contains("Invalid master key field"));
    }

    #[tokio::test]
    async fn update_user_rating_rejects_out_of_range_min_max_and_below_floor() {
        let pool = migrated_pool().await;
        // min_rating outside 0..=5
        assert!(matches!(
            update_user_rating(&pool, HEX_KEY_A.to_string(), 5, 6, 5, 1, 5.0).await,
            Err(MostroError::MostroCantDo(CantDoReason::InvalidRating))
        ));
        // max_rating outside 0..=5
        assert!(matches!(
            update_user_rating(&pool, HEX_KEY_A.to_string(), 5, 0, 9, 1, 5.0).await,
            Err(MostroError::MostroCantDo(CantDoReason::InvalidRating))
        ));
        // last_rating below the MIN_RATING floor (0 < 1)
        assert!(matches!(
            update_user_rating(&pool, HEX_KEY_A.to_string(), 0, 0, 5, 1, 0.0).await,
            Err(MostroError::MostroCantDo(CantDoReason::InvalidRating))
        ));
    }

    // ── restore session ──────────────────────────────────────────────────

    #[tokio::test]
    async fn find_user_orders_by_master_key_validates_and_finds_both_sides() {
        let pool = migrated_pool().await;
        assert!(find_user_orders_by_master_key(&pool, "not-hex")
            .await
            .is_err());

        // One active order as buyer, one as seller, one terminal (excluded).
        insert_order(
            &pool,
            Uuid::new_v4(),
            "buy",
            "active",
            Some(HEX_KEY_A),
            Some(HEX_KEY_B),
            HEX_KEY_A,
            0,
        )
        .await;
        insert_order(
            &pool,
            Uuid::new_v4(),
            "sell",
            "waiting-payment",
            Some(HEX_KEY_B),
            Some(HEX_KEY_A),
            HEX_KEY_A,
            0,
        )
        .await;
        insert_order(
            &pool,
            Uuid::new_v4(),
            "buy",
            "canceled",
            Some(HEX_KEY_A),
            Some(HEX_KEY_B),
            HEX_KEY_A,
            0,
        )
        .await;

        let orders = find_user_orders_by_master_key(&pool, HEX_KEY_A)
            .await
            .unwrap();
        assert_eq!(orders.len(), 2, "terminal orders are excluded");
    }

    #[tokio::test]
    async fn find_user_disputes_by_master_key_validates_and_joins_orders() {
        let pool = migrated_pool().await;
        assert!(find_user_disputes_by_master_key(&pool, "xyz")
            .await
            .is_err());

        let order_id = Uuid::new_v4();
        insert_order(
            &pool,
            order_id,
            "buy",
            "dispute",
            Some(HEX_KEY_A),
            Some(HEX_KEY_B),
            HEX_KEY_A,
            0,
        )
        .await;
        insert_dispute(&pool, order_id, "initiated", None).await;

        let disputes = find_user_disputes_by_master_key(&pool, HEX_KEY_A)
            .await
            .unwrap();
        assert_eq!(disputes.len(), 1);
        assert_eq!(disputes[0].order_id, order_id);
    }

    #[tokio::test]
    async fn restore_session_manager_delivers_background_results() {
        let pool = migrated_pool().await;
        insert_order(
            &pool,
            Uuid::new_v4(),
            "buy",
            "active",
            Some(HEX_KEY_A),
            Some(HEX_KEY_B),
            HEX_KEY_A,
            0,
        )
        .await;

        // Default delegates to new().
        let mut manager = RestoreSessionManager::default();
        // Nothing pending yet.
        assert!(manager.check_results().await.is_none());

        manager
            .start_restore_session(pool.clone(), HEX_KEY_A.to_string())
            .await
            .unwrap();
        let info = manager
            .wait_for_result()
            .await
            .expect("background restore session must deliver");
        assert_eq!(info.restore_orders.len(), 1);
        assert!(info.restore_disputes.is_empty());

        // Invalid master key: worker logs the error, nothing is delivered.
        manager
            .start_restore_session(pool.clone(), "not-hex".to_string())
            .await
            .unwrap();
        // Give the blocking task a moment, then confirm no result arrived.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(manager.check_results().await.is_none());
    }

    // ── connect() ────────────────────────────────────────────────────────

    /// `connect()` reads the database URL from the global settings, which in
    /// the test binary depend on whichever module initialized them first
    /// (canonical `sqlite::memory:` or a default empty URL). Both are
    /// exercised tolerantly: either the pool comes up (in-memory) or a
    /// clean error surfaces — never a panic. Any stray file the in-memory
    /// URL shape creates in the CWD is removed.
    #[tokio::test]
    async fn connect_is_panic_free_under_test_configuration() {
        init_test_settings();
        let first = connect().await;
        let second = connect().await;
        match (&first, &second) {
            (Ok(_), Ok(_)) | (Err(_), Err(_)) => {}
            other => panic!("connect() must behave consistently, got {other:?}"),
        }
        // Clean up the artifact of the "sqlite::memory:" URL shape.
        let stray = std::path::Path::new("sqlite::memory:");
        if stray.exists() {
            let _ = std::fs::remove_file(stray);
        }
    }
}
