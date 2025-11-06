use crate::db::is_user_present;
use crate::util::send_dm;
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

// Handle last_trade_index action
pub async fn last_trade_index(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get requester pubkey (sender of the message)
    let requester_pubkey = event.sender.to_string();

    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    // Check if user is present in the database
    // If not, return a not found error
    let user = match is_user_present(pool, requester_pubkey).await {
        Ok(user) => user,
        Err(_) => {
            return Err(MostroCantDo(CantDoReason::NotFound));
        }
    };

    // Zero should never be returned by the database - because it's related to identity key
    if user.last_trade_index == 0 {
        return Err(MostroCantDo(CantDoReason::InvalidTradeIndex));
    }

    // Build response message embedding the last_trade_index in the trade_index field
    let kind = MessageKind::new(
        None,
        request_id,
        Some(user.last_trade_index),
        Action::LastTradeIndex,
        None,
    );
    let last_trade_index_message = Message::Restore(kind);
    let message_json = last_trade_index_message
        .as_json()
        .map_err(|_| MostroError::MostroInternalErr(ServiceError::MessageSerializationError))?;

    // Print the last trade index message
    tracing::info!(
        "User with pubkey: {} requested last trade index",
        user.pubkey
    );
    tracing::info!("Last trade index: {}", user.last_trade_index);

    // Send DM back to the requester
    if let Err(e) = send_dm(event.sender, my_keys, &message_json, None).await {
        tracing::error!("Error sending DM with last trade index: {:?}", e);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::{Keys, Kind as NostrKind, Timestamp, UnsignedEvent};
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;

    // Helper function to create test keys
    fn create_test_keys() -> Keys {
        Keys::generate()
    }

    // Helper function to create UnwrappedGift for testing
    fn create_test_unwrapped_gift(sender_keys: &Keys) -> UnwrappedGift {
        let keys = create_test_keys();

        let unsigned_event = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            NostrKind::GiftWrap,
            Vec::new(),
            "",
        );
        println!(
            "Creating UnwrappedGift with sender pubkey: {}",
            sender_keys.public_key()
        );

        UnwrappedGift {
            sender: sender_keys.public_key(),
            rumor: unsigned_event,
        }
    }

    // Helper function to set up in-memory test database with user table
    async fn setup_test_db() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();

        // Create the users table schema matching application expectations
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                pubkey CHAR(64) PRIMARY KEY NOT NULL,
                is_admin INTEGER NOT NULL DEFAULT 0,
                admin_password CHAR(64),
                is_solver INTEGER NOT NULL DEFAULT 0,
                is_banned INTEGER NOT NULL DEFAULT 0,
                category INTEGER NOT NULL DEFAULT 0,
                last_trade_index INTEGER NOT NULL DEFAULT 0,
                total_reviews INTEGER NOT NULL DEFAULT 0,
                total_rating REAL NOT NULL DEFAULT 0.0,
                last_rating INTEGER NOT NULL DEFAULT 0,
                max_rating INTEGER NOT NULL DEFAULT 0,
                min_rating INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT 0
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    // Helper function to insert a test user
    async fn insert_test_user(pool: &SqlitePool, pubkey: &str, last_trade_index: i64) {
        sqlx::query(
            r#"
            INSERT INTO users (pubkey, last_trade_index)
            VALUES (?, ?)
            "#,
        )
        .bind(pubkey)
        .bind(last_trade_index)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_last_trade_index_user_not_found() {
        // Setup: Create empty database (no users)
        let pool = setup_test_db().await;
        let sender_keys = create_test_keys();

        // Create test event for non-existent user
        let event = create_test_unwrapped_gift(&sender_keys);

        // Execute function
        let result = last_trade_index(&event, &sender_keys, &pool).await;

        // Should fail because user doesn't exist
        assert!(
            result.is_err(),
            "Should return error when user doesn't exist"
        );

        // Verify it's the right kind of error (user not found)
        match result {
            Err(MostroError::MostroCantDo(CantDoReason::NotFound)) => {
                // Expected error type for user not found
            }
            Err(e) => {
                panic!("Expected MostroInternalErr(DbAccessError), got: {:?}", e);
            }
            Ok(_) => {
                panic!("Should have failed when user doesn't exist");
            }
        }
    }

    #[tokio::test]
    async fn test_last_trade_index_correct_value() {
        // Setup: Create database with multiple users with different trade indexes
        let pool = setup_test_db().await;

        // User 1 with trade_index = 10
        let user1_keys = create_test_keys();
        let user1_pubkey = user1_keys.public_key().to_string();
        insert_test_user(&pool, &user1_pubkey, 10).await;

        // User 2 with trade_index = 99
        let user2_keys = create_test_keys();
        let user2_pubkey = user2_keys.public_key().to_string();
        insert_test_user(&pool, &user2_pubkey, 99).await;

        // Verify that is_user_present returns correct last_trade_index for each user
        let user1 = is_user_present(&pool, user1_pubkey.clone()).await.unwrap();
        assert_eq!(
            user1.last_trade_index, 10,
            "User 1 should have last_trade_index = 10"
        );

        let user2 = is_user_present(&pool, user2_pubkey.clone()).await.unwrap();
        assert_eq!(
            user2.last_trade_index, 99,
            "User 2 should have last_trade_index = 99"
        );

        // Test message construction with correct trade_index
        let message = MessageKind::new(
            None,
            None,
            Some(user1.last_trade_index),
            Action::LastTradeIndex,
            None,
        );

        // Verify the message contains the correct trade_index
        assert_eq!(
            message.trade_index(),
            10i64,
            "Message should contain correct trade_index"
        );
    }

    #[tokio::test]
    async fn test_last_trade_index_zero_value() {
        // Test edge case: user with last_trade_index = 0 (brand new user)
        let pool = setup_test_db().await;
        let sender_keys = create_test_keys();
        let pubkey = sender_keys.public_key().to_string();

        // Insert user with last_trade_index = 0
        insert_test_user(&pool, &pubkey, 0).await;

        // Verify user retrieval works for zero value
        let user = is_user_present(&pool, pubkey.clone()).await.unwrap();
        assert_eq!(
            user.last_trade_index, 0,
            "New user should have last_trade_index = 0"
        );

        // Verify message construction with zero value
        let message = MessageKind::new(
            None,
            None,
            Some(user.last_trade_index),
            Action::LastTradeIndex,
            None,
        );

        assert_eq!(
            message.trade_index(),
            0i64,
            "Message should handle zero trade_index correctly"
        );
    }

    #[tokio::test]
    async fn test_last_trade_index_message_serialization() {
        // Test that MessageKind can be serialized to JSON successfully
        let pool = setup_test_db().await;
        let sender_keys = create_test_keys();
        let pubkey = sender_keys.public_key().to_string();
        let trade_index = 42i64;

        insert_test_user(&pool, &pubkey, trade_index).await;

        let user = is_user_present(&pool, pubkey).await.unwrap();

        // Test message serialization
        let message = MessageKind::new(
            None,
            None,
            Some(user.last_trade_index),
            Action::LastTradeIndex,
            None,
        );

        let json_result = message.as_json();
        assert!(json_result.is_ok(), "Message serialization should succeed");

        // Verify JSON structure
        let json = json_result.unwrap();
        assert!(
            json.contains("last-trade-index"),
            "JSON should contain action field"
        );
        assert!(
            json.contains("trade_index"),
            "JSON should contain trade_index field"
        );
    }
}
