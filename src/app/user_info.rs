use crate::app::context::AppContext;
use crate::db::find_user_by_pubkey;
use crate::util::{peer_reputation, send_dm};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

/// Handles a `user-info` request: answers the reputation Mostro holds for the
/// proven identity with a DM to the requesting trade key. Read-only, publishes
/// nothing.
///
/// An identity Mostro has no record of gets zeros and no `since`, not
/// `not_found`. A request without an identity proof (full privacy, identity
/// == trade key) gets `cant-do` `reputation_identity_required`.
pub async fn user_info(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    if event.identity == event.sender {
        return Err(MostroCantDo(CantDoReason::ReputationIdentityRequired));
    }

    let user = find_user_by_pubkey(ctx.pool(), event.identity.to_string()).await?;

    let response = user_info_message(
        user.as_ref(),
        msg.get_inner_message_kind().request_id,
        Timestamp::now().as_secs(),
    );
    let message_json = response
        .as_json()
        .map_err(|_| MostroError::MostroInternalErr(ServiceError::MessageSerializationError))?;

    if let Err(e) = send_dm(event.sender, my_keys, &message_json, None).await {
        tracing::error!("Error sending message with user info: {:?}", e);
    }

    Ok(())
}

/// The `user-info` reply: the same `UserInfo` a counterpart gets in `peer`.
fn user_info_message(user: Option<&User>, request_id: Option<u64>, now: u64) -> Message {
    Message::Restore(MessageKind::new(
        None,
        request_id,
        None,
        Action::UserInfo,
        Some(Payload::UserInfo(peer_reputation(user, now))),
    ))
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

    fn create_test_unwrapped_message(identity: &Keys, trade: &Keys) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::Restore(MessageKind::new(
                None,
                Some(1),
                None,
                Action::UserInfo,
                None,
            )),
            signature: None,
            sender: trade.public_key(),
            identity: identity.public_key(),
            created_at: Timestamp::now(),
        }
    }

    fn reply_info(msg: &Message) -> UserInfo {
        match &msg.get_inner_message_kind().payload {
            Some(Payload::UserInfo(info)) => info.clone(),
            other => panic!("expected a user_info payload, got {other:?}"),
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

    fn test_ctx(pool: SqlitePool) -> AppContext {
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool))
            .with_settings(test_settings())
            .build()
    }

    #[tokio::test]
    async fn full_privacy_request_gets_reputation_identity_required() {
        let ctx = test_ctx(setup_test_db().await);
        let keys = create_test_keys();
        let event = create_test_unwrapped_message(&keys, &keys);

        let result = user_info(&ctx, event.message.clone(), &event, &keys).await;

        assert_eq!(
            result.unwrap_err(),
            MostroCantDo(CantDoReason::ReputationIdentityRequired)
        );
    }

    #[tokio::test]
    async fn unknown_identity_is_answered_not_rejected() {
        let ctx = test_ctx(setup_test_db().await);
        let identity = create_test_keys();
        let event = create_test_unwrapped_message(&identity, &create_test_keys());

        let result = user_info(&ctx, event.message.clone(), &event, &identity).await;

        assert!(
            result.is_ok(),
            "unknown identity must not be an error: {result:?}"
        );
    }

    #[tokio::test]
    async fn known_identity_is_answered() {
        let pool = setup_test_db().await;
        let identity = create_test_keys();
        let created_at = Timestamp::now().as_secs() as i64 - 10 * 86400;
        insert_test_user(
            &pool,
            &identity.public_key().to_string(),
            4.5,
            12,
            created_at,
        )
        .await;
        let ctx = test_ctx(pool);
        let event = create_test_unwrapped_message(&identity, &create_test_keys());

        let result = user_info(&ctx, event.message.clone(), &event, &identity).await;

        assert!(
            result.is_ok(),
            "known identity must be answered: {result:?}"
        );
    }

    #[tokio::test]
    async fn users_lookup_error_is_propagated_not_answered_as_unknown() {
        let pool = setup_test_db().await;
        let identity = create_test_keys();
        let event = create_test_unwrapped_message(&identity, &create_test_keys());
        let ctx = test_ctx(pool.clone());
        pool.close().await;

        let result = user_info(&ctx, event.message.clone(), &event, &identity).await;

        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::DbAccessError(_)))
            ),
            "a database failure must not be answered as an unknown identity: {result:?}"
        );
    }

    #[test]
    fn reply_carries_the_peer_reputation_and_echoes_request_id() {
        let created_at = 1_700_000_000 + 3600;
        let now = 1_700_000_000 + 10 * 86400;
        let user = User {
            pubkey: "abc".to_string(),
            total_rating: 4.8,
            total_reviews: 23,
            created_at,
            ..Default::default()
        };

        let msg = user_info_message(Some(&user), Some(123456), now as u64);

        assert!(msg.verify());
        let kind = msg.get_inner_message_kind();
        assert_eq!(kind.action, Action::UserInfo);
        assert_eq!(kind.request_id, Some(123456));
        assert!(kind.id.is_none());
        let info = reply_info(&msg);
        assert_eq!(info.rating, 4.8);
        assert_eq!(info.reviews, 23);
        assert_eq!(info.operating_days, 9);
        assert_eq!(info.since, Some(1_699_920_000));

        let json: serde_json::Value = serde_json::from_str(&msg.as_json().unwrap()).unwrap();
        assert_eq!(json["restore"]["action"], "user-info");
        assert_eq!(json["restore"]["request_id"], 123456);
        assert_eq!(json["restore"]["payload"]["user_info"]["reviews"], 23);
        assert_eq!(
            json["restore"]["payload"]["user_info"]["since"],
            1_699_920_000
        );
    }

    #[test]
    fn reply_for_unknown_identity_is_zeros_without_since() {
        let msg = user_info_message(None, None, 1_700_000_000);

        assert!(msg.verify());
        let info = reply_info(&msg);
        assert_eq!(info.rating, 0.0);
        assert_eq!(info.reviews, 0);
        assert_eq!(info.operating_days, 0);
        assert!(info.since.is_none());

        let json: serde_json::Value = serde_json::from_str(&msg.as_json().unwrap()).unwrap();
        assert_eq!(json["restore"]["action"], "user-info");
        assert!(json["restore"]["payload"]["user_info"]
            .get("since")
            .is_none());
    }
}
