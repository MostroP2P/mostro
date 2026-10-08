use crate::app::context::AppContext;
use crate::db::is_user_present;
use crate::util::{peer_reputation, send_dm};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

/// Handles a `get-user-info` request: return the requester's own reputation
/// snapshot (`UserInfo`) when Mostro knows this identity.
///
/// Mirrors [`super::last_trade_index::last_trade_index`] in transport and
/// reply shape, but does not care about trade index. Unknown identities get
/// a success reply with `payload: None` (optional `UserInfo`), not `CantDo`.
pub async fn get_user_info(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    let requester_pubkey = event.identity.to_string();
    let trade_key = event.sender;
    let request_id = msg.get_inner_message_kind().request_id;

    let payload = match is_user_present(pool, requester_pubkey).await {
        Ok(user) => {
            let reputation = peer_reputation(Some(&user), Timestamp::now().as_secs());
            Some(Payload::Peer(Peer {
                pubkey: user.pubkey.clone(),
                reputation: Some(reputation),
            }))
        }
        Err(_) => None,
    };

    let kind = MessageKind::new(None, request_id, None, Action::GetUserInfo, payload);
    let response = Message::Restore(kind);
    let message_json = response
        .as_json()
        .map_err(|_| MostroError::MostroInternalErr(ServiceError::MessageSerializationError))?;

    tracing::info!(
        "User with identity {} requested own user info",
        event.identity
    );

    if let Err(e) = send_dm(trade_key, my_keys, &message_json, None).await {
        tracing::error!("Error sending message with user info: {:?}", e);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::prelude::{Keys, Timestamp};
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;

    fn create_test_keys() -> Keys {
        Keys::generate()
    }

    fn create_test_unwrapped_message(sender_keys: &Keys) -> UnwrappedMessage {
        let trade = create_test_keys();
        UnwrappedMessage {
            message: Message::Restore(MessageKind::new(
                None,
                Some(1),
                None,
                Action::GetUserInfo,
                None,
            )),
            signature: None,
            sender: trade.public_key(),
            identity: sender_keys.public_key(),
            created_at: Timestamp::now(),
        }
    }

    async fn setup_test_db() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();

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

    async fn insert_test_user(
        pool: &SqlitePool,
        pubkey: &str,
        total_rating: f64,
        total_reviews: i64,
        created_at: i64,
    ) {
        sqlx::query(
            r#"
            INSERT INTO users (pubkey, total_rating, total_reviews, created_at, last_trade_index)
            VALUES (?, ?, ?, ?, 1)
            "#,
        )
        .bind(pubkey)
        .bind(total_rating)
        .bind(total_reviews)
        .bind(created_at)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_get_user_info_unknown_user_succeeds() {
        let _ = crate::config::MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
        let pool = setup_test_db().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let sender_keys = create_test_keys();
        let event = create_test_unwrapped_message(&sender_keys);
        let kind = MessageKind::new(None, Some(9), None, Action::GetUserInfo, None);

        let result = get_user_info(&ctx, Message::Restore(kind), &event, &sender_keys).await;

        assert!(
            result.is_ok(),
            "unknown identity must succeed with optional payload: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_get_user_info_known_user_succeeds() {
        let _ = crate::config::MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
        let pool = setup_test_db().await;
        let sender_keys = create_test_keys();
        let pubkey = sender_keys.public_key().to_string();
        let created_at = Timestamp::now().as_secs() as i64 - 10 * 86400;
        insert_test_user(&pool, &pubkey, 4.5, 12, created_at).await;

        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let event = create_test_unwrapped_message(&sender_keys);
        let kind = MessageKind::new(None, Some(3), None, Action::GetUserInfo, None);

        let result = get_user_info(&ctx, Message::Restore(kind), &event, &sender_keys).await;
        assert!(result.is_ok(), "known user path must succeed: {result:?}");
    }

    #[test]
    fn test_get_user_info_response_payload_shape() {
        let reputation = peer_reputation(
            Some(&User {
                pubkey: "abc".to_string(),
                total_rating: 4.5,
                total_reviews: 12,
                created_at: 1_700_000_000,
                ..Default::default()
            }),
            1_700_000_000 + 10 * 86400,
        );
        let kind = MessageKind::new(
            None,
            Some(1),
            None,
            Action::GetUserInfo,
            Some(Payload::Peer(Peer {
                pubkey: "abc".to_string(),
                reputation: Some(reputation.clone()),
            })),
        );
        assert!(kind.verify());
        let json = Message::Restore(kind).as_json().unwrap();
        assert!(json.contains("get-user-info"));
        assert!(json.contains("peer"));
        assert_eq!(reputation.rating, 4.5);
        assert_eq!(reputation.reviews, 12);
        assert_eq!(reputation.operating_days, 10);
        assert!(reputation.since.is_some());
    }

    #[test]
    fn test_get_user_info_none_payload_verifies() {
        let kind = MessageKind::new(None, None, None, Action::GetUserInfo, None);
        assert!(kind.verify());
        let json = Message::Restore(kind).as_json().unwrap();
        assert!(json.contains("get-user-info"));
    }
}
