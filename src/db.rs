#[cfg(feature = "startos")]
use crate::cli::Cli;
use crate::config::settings::Settings;
use crate::config::MOSTRO_DB_PASSWORD;
use argon2::password_hash::rand_core::OsRng;
use argon2::{password_hash::SaltString, Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
#[cfg(feature = "startos")]
use clap::Parser;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use rpassword::read_password;
use secrecy::zeroize::Zeroize;
use secrecy::{ExposeSecret, SecretString};
use sqlx::pool::Pool;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqlitePool};
use std::fs::{set_permissions, Permissions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

// Constants for status filtering used across restore session functions
const EXCLUDED_ORDER_STATUSES: &str = "'expired','success','canceled','dispute','canceledbyadmin','completedbyadmin','settledbyadmin','cooperativelycanceled'";
const ACTIVE_DISPUTE_STATUSES: &str = "'initiated','in-progress'";

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn restrict_file_permissions(path: &Path) -> Result<(), MostroError> {
    #[cfg(unix)]
    {
        let perms = Permissions::from_mode(0o600);
        set_permissions(path, perms)
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    }

    #[cfg(windows)]
    {
        // Optional: could integrate with `winapi` or use a placeholder
        println!("⚠️ Skipping permission change on Windows. Set it manually if needed.");
    }

    Ok(())
}

/// Password strength requirements struct
struct PasswordRequirements {
    min_length: usize,
    requires_uppercase: bool,
    requires_lowercase: bool,
    requires_digit: bool,
    requires_special: bool,
}

impl Default for PasswordRequirements {
    fn default() -> Self {
        Self {
            min_length: 12, // Recommended minimum length
            requires_uppercase: true,
            requires_lowercase: true,
            requires_digit: true,
            requires_special: true,
        }
    }
}

impl PasswordRequirements {
    fn validate(&self, password: &str) -> Vec<String> {
        let mut failures = Vec::new();

        if password.len() < self.min_length {
            failures.push(format!(
                "Password must be at least {} characters long",
                self.min_length
            ));
        }

        if self.requires_uppercase && !password.chars().any(|c| c.is_uppercase()) {
            failures.push("Password must contain at least one uppercase letter".to_string());
        }

        if self.requires_lowercase && !password.chars().any(|c| c.is_lowercase()) {
            failures.push("Password must contain at least one lowercase letter".to_string());
        }

        if self.requires_digit && !password.chars().any(|c| c.is_ascii_digit()) {
            failures.push("Password must contain at least one number".to_string());
        }

        if self.requires_special && !password.chars().any(|c| !c.is_alphanumeric()) {
            failures.push("Password must contain at least one special character".to_string());
        }

        // If password is empty, clear failures
        if password.is_empty() {
            // Empty password is allowed to support optional encryption
            failures.clear();
        }

        failures
    }

    fn is_strong_password(&self, password: &str) -> bool {
        match self.validate(password).is_empty() {
            true => true,
            false => {
                println!("\nPassword is not strong enough:");
                for failure in self.validate(password) {
                    println!("- {}", failure);
                }
                false
            }
        }
    }
}

fn check_password_hash(password_hash: &PasswordHash) -> Result<bool, MostroError> {
    // Get user input password to check against stored hash
    print!("Enter database password: ");
    std::io::stdout().flush().unwrap();
    // Simulate a delay in password input to avoid timing attacks
    let random_delay = rand::random::<u16>() % 1000;
    let password = read_password()
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Simulate a delay in password input to avoid timing attacks
    std::thread::sleep(std::time::Duration::from_millis(
        100_u64 + random_delay as u64,
    ));

    if Argon2::default()
        .verify_password(password.as_bytes(), password_hash)
        .is_ok()
    {
        if MOSTRO_DB_PASSWORD.set(SecretString::from(password)).is_ok() {
            Ok(true)
        } else {
            Err(MostroInternalErr(ServiceError::DbAccessError(
                "Failed to save password".to_string(),
            )))
        }
    } else {
        Err(MostroInternalErr(ServiceError::DbAccessError(
            "Invalid password".to_string(),
        )))
    }
}

/// Verify the provided password against the stored admin hash and set it in memory on success.
/// This is a non-interactive variant used by RPC.
pub async fn verify_and_set_db_password(
    pool: &SqlitePool,
    password: String,
) -> Result<(), MostroError> {
    // Fetch stored admin password hash
    let Some(argon2_hash) = get_admin_password(pool).await? else {
        return Err(MostroInternalErr(ServiceError::DbAccessError(
            "Database encryption not enabled".to_string(),
        )));
    };

    let parsed_hash = PasswordHash::new(&argon2_hash)
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok()
    {
        // Save the password in memory if not set. If already set, keep the existing value.
        let _ = MOSTRO_DB_PASSWORD.set(SecretString::from(password));
        Ok(())
    } else {
        Err(MostroInternalErr(ServiceError::DbAccessError(
            "Invalid password".to_string(),
        )))
    }
}

fn password_instructions(password_requirements: &PasswordRequirements) {
    // Print password requirements
    println!("\nHey Mostro admin insert a password to encrypt the database:");
    println!(
        "- At least {} characters long",
        password_requirements.min_length
    );
    println!("- At least one uppercase letter");
    println!("- At least one lowercase letter");
    println!("- At least one number");
    println!("- At least one special character");
}

async fn get_user_password() -> Result<(), MostroError> {
    // Password requirements settings
    let password_requirements = PasswordRequirements::default();
    // If password is already set, check if it's strong enough
    // If not, prompt user to enter a new password
    // password here is set from command line argument --password
    if let Some(password) = MOSTRO_DB_PASSWORD.get() {
        if password_requirements.is_strong_password(password.expose_secret()) {
            println!("Database password already set");
            return Ok(());
        } else {
            println!("Database password is not strong enough");
            password_instructions(&password_requirements);
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                "Database password is not strong enough".to_string(),
            )));
        }
    }

    #[cfg(feature = "startos")]
    {
        let cli = Cli::parse();
        if cli.cleartext {
            tracing::info!("No password encryption will be used for database");
            return Ok(());
        }
    }

    // New database - need password creation
    loop {
        // First password entry
        print!("\nEnter new database password (Press enter to skip encryption): ");

        // get a random delay to avoid timing attacks
        let random_delay = rand::random::<u16>() % 1000;

        std::io::stdout().flush().unwrap();
        let password = read_password()
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        // Simulate a delay in password input to avoid timing attacks
        std::thread::sleep(std::time::Duration::from_millis(
            100_u64 + random_delay as u64,
        ));

        // Check password strength
        if !password_requirements.is_strong_password(&password) {
            continue;
        }
        if password.is_empty() {
            print!("Press enter to skip password");
        } else {
            // Confirm password
            print!("Confirm database password: ");
        }

        std::io::stdout().flush().unwrap();
        let mut confirm_password = read_password().map_err(|_| {
            MostroInternalErr(ServiceError::IOError("Failed to read password".to_string()))
        })?;

        if password == confirm_password {
            // zeroize confirm password in ram
            confirm_password.zeroize();
            if password.is_empty() {
                println!("Password skipped!!");
                break;
            } else {
                // Save password in static variable using OnceLock and SecretString to avoid exposing the password in memory and logs
                if MOSTRO_DB_PASSWORD.set(SecretString::from(password)).is_ok() {
                    break;
                } else {
                    println!("Failed to save password please try again");
                }
            }
        } else {
            println!("Passwords do not match. Please try again.");
        }
    }
    Ok(())
}

/// Migrates legacy disputes table by removing deprecated buyer_token and seller_token columns if present.
///
/// This function checks if the deprecated token columns exist in the disputes table
/// and removes them to maintain compatibility with the updated mostro-core.
/// The function handles both cases where columns exist (legacy databases) and don't exist (newer databases).
async fn migrate_remove_token_columns(pool: &SqlitePool) -> Result<(), MostroError> {
    // Check if buyer_token column exists
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

    // Check if seller_token column exists
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

    // Remove buyer_token column if it exists
    if buyer_token_exists {
        tracing::info!("Removing deprecated buyer_token column from disputes table");
        sqlx::query("ALTER TABLE disputes DROP COLUMN buyer_token")
            .execute(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        tracing::info!("Successfully removed buyer_token column");
    }

    // Remove seller_token column if it exists
    if seller_token_exists {
        tracing::info!("Removing deprecated seller_token column from disputes table");
        sqlx::query("ALTER TABLE disputes DROP COLUMN seller_token")
            .execute(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        tracing::info!("Successfully removed seller_token column");
    }

    if !buyer_token_exists && !seller_token_exists {
        tracing::debug!("No deprecated token columns found in disputes table - migration not needed");
    }

    Ok(())
}

pub async fn connect() -> Result<Arc<Pool<Sqlite>>, MostroError> {
    // Get mostro settings
    let db_settings = Settings::get_db();
    let db_url = &db_settings.url;
    let tmp = db_url.replace("sqlite://", "");
    let db_path = Path::new(&tmp);

    let conn = if !db_path.exists() {
        //Create new database file
        let _file = std::fs::File::create_new(db_path)
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        // Restrict file permissions only owner can read and write
        // TODO: check if this is works on windows
        restrict_file_permissions(db_path)
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

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
                        
                        // Get user password
                        match get_user_password().await {
                            Ok(_) => {}
                            Err(e) => {
                                tracing::error!("Failed to set up database password: {}", e);
                                println!(
                                    "Failed to set up database password. Removing database file."
                                );
                                if let Err(cleanup_err) = std::fs::remove_file(db_path) {
                                    tracing::error!(
                                        error = %cleanup_err,
                                        path = %db_path.display(),
                                        "Failed to clean up database file"
                                    );
                                }
                                std::process::exit(1);
                            }
                        }
                        // Save admin password hash securely
                        if let Some(password) = MOSTRO_DB_PASSWORD.get() {
                            store_password_hash(password, &pool).await.map_err(|e| {
                                MostroInternalErr(ServiceError::DbAccessError(e.to_string()))
                            })?;
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

        // Opening existing database - allow maximum 3 attempts
        let max_attempts = 3;
        let mut attempts = 0;

        if MOSTRO_DB_PASSWORD.get().is_none() {
            while let Some(argon2_hash) = get_admin_password(&conn).await? {
                // Database already exists - and yet opened
                let parsed_hash = PasswordHash::new(&argon2_hash)
                    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
                if check_password_hash(&parsed_hash).is_ok() {
                    break;
                } else {
                    attempts += 1;
                    println!("Wrong password, attempts: {}", attempts);
                    if attempts >= max_attempts {
                        println!("Maximum password attempts exceeded!!");
                        std::process::exit(1);
                    }
                }
            }
        }

        // Run legacy column migration for existing databases
        if let Err(e) = migrate_remove_token_columns(&conn).await {
            tracing::error!("Failed to migrate token columns on existing database: {}", e);
            return Err(e);
        }

        conn
    };
    Ok(Arc::new(conn))
}

// You'll need to implement these functions to store and verify the password hash
async fn store_password_hash(
    password: &SecretString,
    pool: &SqlitePool,
) -> Result<(), MostroError> {
    // Generate a random salt
    let salt = SaltString::generate(&mut OsRng);

    // Configure Argon2 parameters
    let argon2 = Argon2::default();

    // Derive the key
    let key = argon2
        .hash_password(password.expose_secret().as_bytes(), &salt)
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
        .to_string();

    // Get mostro keys
    let my_keys = crate::util::get_keys()
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Store the key and salt securely (e.g., in a file or database)
    let new_user: User = User {
        pubkey: my_keys.public_key.to_string(),
        is_admin: 1,
        admin_password: Some(key),
        ..Default::default()
    };
    if let Err(e) = add_new_user(pool, new_user).await {
        tracing::error!("Error creating new user: {}", e);
        return Err(MostroError::MostroCantDo(CantDoReason::CantCreateUser));
    }

    Ok(())
}

pub async fn edit_buyer_pubkey_order(
    pool: &SqlitePool,
    order_id: Uuid,
    buyer_pubkey: Option<String>,
) -> Result<bool, MostroError> {
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
    .execute(pool)
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
    .execute(pool)
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
    .execute(pool)
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
    .execute(pool)
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
    .execute(pool)
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
    .bind(created_at.to_string())
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Return the public key not encrypted
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

    let result = sqlx::query!(
        r#"
            UPDATE users SET last_trade_index = ?1 WHERE pubkey = ?2
        "#,
        trade_index,
        public_key,
    )
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

    // Check if database is encrypted
    if MOSTRO_DB_PASSWORD.get().is_some() {
        let orders_to_check: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT {} FROM orders WHERE status = ?",
            master_key_field
        ))
        .bind(status)
        .fetch_all(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        // search for orders with the same pubkey
        for master_key in orders_to_check {
            // Decrypt master pubkey
            let master_pubkey_decrypted =
                CryptoUtils::decrypt_data(master_key, MOSTRO_DB_PASSWORD.get())
                    .map_err(MostroInternalErr)?;
            if master_pubkey_decrypted == pubkey {
                return Ok(true);
            }
        }
        Ok(false)
    }
    // if not encrypted, use the default search
    else {
        let exists = sqlx::query_scalar::<_, bool>(&format!(
            "SELECT EXISTS (SELECT 1 FROM orders WHERE {} = ? AND status = ?)",
            master_key_field
        ))
        .bind(pubkey)
        .bind(status)
        .fetch_one(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        Ok(exists)
    }
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

/// Find all orders for a user by their master key (for restore session)
/// This function efficiently handles both encrypted and non-encrypted databases
/// Uses constants for excluded statuses to maintain consistency across queries
pub async fn find_user_orders_by_master_key(
    pool: &SqlitePool,
    master_key: &str,
) -> Result<Vec<RestoredOrdersInfo>, MostroError> {
    // Validate public key format (32-bytes hex)
    if !master_key.chars().all(|c| c.is_ascii_hexdigit()) || master_key.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // Check if database is encrypted
    if MOSTRO_DB_PASSWORD.get().is_some() {
        // For encrypted databases, we need to fetch and decrypt
        // But we can optimize by filtering out completed/success orders first
        let sql_query = format!(
            r#"
            SELECT id, status, master_buyer_pubkey, master_seller_pubkey, 
                trade_index_buyer, trade_index_seller
            FROM orders 
            WHERE status NOT IN ({})
              AND (master_buyer_pubkey IS NOT NULL OR master_seller_pubkey IS NOT NULL)
            "#,
            EXCLUDED_ORDER_STATUSES
        );

        let active_orders = sqlx::query_as::<_, RestoredOrderHelper>(&sql_query)
            .fetch_all(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        tracing::info!("Total orders possibly matching: {}", active_orders.len());
        // Filter orders that match the master key
        let mut matching_orders = Vec::new();
        let total_time = Instant::now();

        for order in active_orders {
            // Check master_buyer_pubkey
            // Decrypt both keys upfront
            let timer = Instant::now();
            let decrypted_master_seller_key = order.master_seller_pubkey.as_ref().and_then(|enc| {
                CryptoUtils::decrypt_data(enc.to_string(), MOSTRO_DB_PASSWORD.get()).ok()
            });
            let decrypted_master_buyer_key = order.master_buyer_pubkey.as_ref().and_then(|enc| {
                CryptoUtils::decrypt_data(enc.to_string(), MOSTRO_DB_PASSWORD.get()).ok()
            });
            let involved_key = if decrypted_master_seller_key.as_deref() == Some(master_key) {
                order.trade_index_seller
            } else if decrypted_master_buyer_key.as_deref() == Some(master_key) {
                order.trade_index_buyer
            } else {
                None
            };
            if let Some(involved_key) = involved_key {
                matching_orders.push(RestoredOrdersInfo {
                    order_id: order.id,
                    trade_index: involved_key,
                    status: order.status,
                });
            }
            tracing::info!("Time taken to process order: {:?}", timer.elapsed());
            tracing::info!("Total time taken: {:?}", total_time.elapsed());
        }

        Ok(matching_orders)
    } else {
        // For non-encrypted databases, use direct SQL queries
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
        let orders = sqlx::query_as::<_, RestoredOrdersInfo>(&sql_query)
            .bind(master_key)
            .bind(master_key)
            .fetch_all(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        Ok(orders)
    }
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

    // Check if database is encrypted
    if MOSTRO_DB_PASSWORD.get().is_some() {
        // For encrypted databases, we need to check each dispute's associated order
        let mut matching_disputes = Vec::new();

        // Single JOIN query - adapted from your non-encrypted approach
        let sql_query = format!(
            r#"
            SELECT 
                d.id AS dispute_id,
                d.order_id AS order_id,
                d.status AS dispute_status,
                o.master_buyer_pubkey,
                o.master_seller_pubkey,
                o.trade_index_buyer,
                o.trade_index_seller
            FROM disputes d
            JOIN orders o ON d.order_id = o.id
            WHERE d.status IN ({})
            "#,
            ACTIVE_DISPUTE_STATUSES
        );

        let dispute_order_rows = sqlx::query_as::<_, RestoredDisputeHelper>(&sql_query)
            .fetch_all(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        tracing::info!(
            "Total disputes possibly matching: {}",
            dispute_order_rows.len()
        );
        let total_time = Instant::now();
        // Process all rows in memory (no more DB calls!)
        for dispute in dispute_order_rows {
            // Decrypt both keys upfront
            let timer = Instant::now();
            let decrypted_master_seller_key =
                dispute.master_seller_pubkey.as_ref().and_then(|enc| {
                    CryptoUtils::decrypt_data(enc.to_string(), MOSTRO_DB_PASSWORD.get()).ok()
                });
            let decrypted_master_buyer_key = dispute.master_buyer_pubkey.as_ref().and_then(|enc| {
                CryptoUtils::decrypt_data(enc.to_string(), MOSTRO_DB_PASSWORD.get()).ok()
            });

            // Check if user is involved and get trade_index (same logic as before)
            let involved_key = if decrypted_master_seller_key.as_deref() == Some(master_key) {
                dispute.trade_index_seller
            } else if decrypted_master_buyer_key.as_deref() == Some(master_key) {
                dispute.trade_index_buyer
            } else {
                None
            };

            if let Some(involved_key) = involved_key {
                matching_disputes.push(RestoredDisputesInfo {
                    dispute_id: dispute.dispute_id,
                    order_id: dispute.order_id,
                    trade_index: involved_key,
                    status: dispute.dispute_status,
                });
            }
            tracing::info!("Time taken to process dispute: {:?}", timer.elapsed());
            tracing::info!("Total time taken: {:?}", total_time.elapsed());
        }
        Ok(matching_disputes)
    } else {
        // For non-encrypted databases, we can use a more efficient approach
        // by joining with orders table
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
                d.status AS status
            FROM disputes d
            JOIN orders o ON d.order_id = o.id
            WHERE (o.master_buyer_pubkey = ? OR o.master_seller_pubkey = ?)
                AND d.status IN ({})
            "#,
            ACTIVE_DISPUTE_STATUSES
        );
        let restore_disputes = sqlx::query_as::<_, RestoredDisputesInfo>(&sql_query)
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
}

/// The actual work function that does the heavy decryption
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
        let (sender, receiver) = tokio::sync::mpsc::channel(100);
        Self { sender, receiver }
    }

    /// Start a restore session background task
    pub async fn start_restore_session(
        &self,
        pool: SqlitePool,
        master_key: String,
    ) -> Result<(), MostroError> {
        let sender = self.sender.clone();

        // Use spawn_blocking for CPU-intensive decryption work
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
    use argon2::password_hash::SaltString;
    use mostro_core::prelude::*;
    use secrecy::SecretString;
    use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
    use sqlx::Error;
    use std::collections::HashSet; // Import HashSet for the test
    use tokio::time::Instant; // Use sqlx::Error for the Result return type

    const TEST_DB_URL: &str = "sqlite::memory:"; // In-memory database for tests
    const SECRET_PASSWORD: &str = "test_password"; // Example password for encryption

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

    #[tokio::test]
    async fn test_fetch_string_column_scalar() {
        // 1. Setup: Create in-memory DB and table
        let pool = setup_db().await.unwrap();
        println!("In-memory database and table created for test.");

        // 2. Populate: Insert 100 entries
        let total_entries = 20;

        // Use a SecretString for the password
        let password = SecretString::from(SECRET_PASSWORD);
        let mut salt_vec: Vec<SaltString> = vec![];
        let salt_base = b"1H/aaYsf8&asduA";
        for i in 0..total_entries {
            let salt = format!("{}{}", String::from_utf8_lossy(salt_base), i % 5);
            salt_vec.push(SaltString::encode_b64(salt.as_bytes()).unwrap());
        }

        println!("Inserting {} entries...", total_entries);
        // Prepare batch values
        let mut query_builder = String::from("INSERT INTO items (id, value) VALUES ");
        let mut params = Vec::new();

        for i in 0..total_entries {
            let value_string = format!("Entry {}", i % 5);
            println!("Inserting value : {:?}", value_string);
            let salt = salt_vec[i % 5].clone();
            let encrypted_value =
                CryptoUtils::store_encrypted(&value_string, Some(&password), Some(salt)).unwrap();

            if i > 0 {
                query_builder.push_str(", ");
            }
            query_builder.push_str(&format!("({}, ?)", i));
            params.push(encrypted_value);
        }

        // Execute batch insert
        let mut query = sqlx::query(&query_builder);
        for param in params {
            query = query.bind(param);
        }
        query.execute(&pool).await.unwrap();
        println!("Entries inserted.");

        // 3. Fetch: Get the 'value' column using query_scalar
        println!("Fetching 'value' column...");
        let sql = "SELECT value FROM items ORDER BY id"; // Order to make assertion predictable

        let fetched_values: Vec<String> = sqlx::query_scalar(sql)
            .fetch_all(&pool) // Fetch all results into Vec<String>
            .await
            .unwrap();

        let mut hash_set_values: HashSet<String> = HashSet::new();
        for value in fetched_values {
            let interval = Instant::now();
            let value_decrypted = CryptoUtils::decrypt_data(value, Some(&password)).unwrap();
            println!(
                "Time taken to decrypt: {:?} ms",
                interval.elapsed().as_millis()
            ); // Print elapsed time
            hash_set_values.insert(value_decrypted);
        }

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
}
