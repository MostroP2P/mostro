use crate::app::context::AppContext;
use crate::db::add_new_user;
use crate::util::send_dm;
use mostro_core::prelude::*;
use mostro_core::user::User;
use nostr_sdk::prelude::*;
use tracing::{error, info};

pub const SOLVER_CATEGORY_READ_ONLY: i64 = 1;
pub const SOLVER_CATEGORY_READ_WRITE: i64 = 2;

fn parse_solver_payload(payload: &Payload) -> Result<(String, i64), MostroError> {
    let raw = match payload {
        Payload::TextMessage(p) => p.trim(),
        _ => return Err(MostroCantDo(CantDoReason::InvalidTextMessage)),
    };

    if raw.is_empty() {
        return Err(MostroCantDo(CantDoReason::InvalidTextMessage));
    }

    let mut parts = raw.split(':');
    let npub = parts
        .next()
        .ok_or(MostroCantDo(CantDoReason::InvalidTextMessage))?
        .trim();

    if npub.is_empty() {
        return Err(MostroCantDo(CantDoReason::InvalidTextMessage));
    }

    let category = match parts.next().map(str::trim) {
        None => SOLVER_CATEGORY_READ_WRITE,
        Some("read") => SOLVER_CATEGORY_READ_ONLY,
        Some("read-write") | Some("write") => SOLVER_CATEGORY_READ_WRITE,
        Some("") | Some(_) => return Err(MostroCantDo(CantDoReason::InvalidParameters)),
    };

    if parts.next().is_some() {
        return Err(MostroCantDo(CantDoReason::InvalidParameters));
    }

    Ok((npub.to_string(), category))
}

pub async fn admin_add_solver_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    let request_id = msg.get_inner_message_kind().request_id;

    let inner_message = msg.get_inner_message_kind();
    let payload = inner_message
        .payload
        .as_ref()
        .ok_or(MostroCantDo(CantDoReason::InvalidTextMessage))?;

    if event.identity != my_keys.public_key() {
        return Err(MostroInternalErr(ServiceError::InvalidPubkey));
    }

    let trade_index = inner_message.trade_index.unwrap_or(0);
    let (npubkey, category) = parse_solver_payload(payload)?;
    let public_key = PublicKey::from_bech32(&npubkey)
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;

    let user = User::new(public_key.to_string(), 0, 1, 0, category, trade_index);

    match add_new_user(pool, user).await {
        Ok(r) => info!("Solver added: {} with category {}", r, category),
        Err(ee) => {
            error!("Error creating solver: {:#?}", ee);
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                ee.to_string(),
            )));
        }
    }

    let message = Message::new_dispute(None, request_id, None, Action::AdminAddSolver, None);
    let message = message
        .as_json()
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;

    send_dm(event.sender, my_keys, &message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_solver_payload, SOLVER_CATEGORY_READ_ONLY, SOLVER_CATEGORY_READ_WRITE};
    use mostro_core::error::CantDoReason;
    use mostro_core::message::Payload;

    #[test]
    fn parse_solver_payload_defaults_to_read_write() {
        let (npub, category) =
            parse_solver_payload(&Payload::TextMessage("npub1test".to_string())).unwrap();
        assert_eq!(npub, "npub1test");
        assert_eq!(category, SOLVER_CATEGORY_READ_WRITE);
    }

    #[test]
    fn parse_solver_payload_accepts_read_only() {
        let (_, category) =
            parse_solver_payload(&Payload::TextMessage("npub1test:read".to_string())).unwrap();
        assert_eq!(category, SOLVER_CATEGORY_READ_ONLY);
    }

    #[test]
    fn parse_solver_payload_accepts_read_write_aliases() {
        let (_, category) =
            parse_solver_payload(&Payload::TextMessage("npub1test:read-write".to_string()))
                .unwrap();
        assert_eq!(category, SOLVER_CATEGORY_READ_WRITE);

        let (_, category) =
            parse_solver_payload(&Payload::TextMessage("npub1test:write".to_string())).unwrap();
        assert_eq!(category, SOLVER_CATEGORY_READ_WRITE);
    }

    #[test]
    fn parse_solver_payload_rejects_invalid_permission() {
        let err =
            parse_solver_payload(&Payload::TextMessage("npub1test:admin".to_string())).unwrap_err();
        assert_eq!(
            err,
            mostro_core::error::MostroError::MostroCantDo(CantDoReason::InvalidParameters)
        );
    }

    #[test]
    fn parse_solver_payload_rejects_empty_permission_token() {
        let err =
            parse_solver_payload(&Payload::TextMessage("npub1test:".to_string())).unwrap_err();
        assert_eq!(
            err,
            mostro_core::error::MostroError::MostroCantDo(CantDoReason::InvalidParameters)
        );
    }

    #[test]
    fn parse_solver_payload_rejects_non_text_payload() {
        let err = parse_solver_payload(&Payload::Dispute(uuid::Uuid::new_v4(), None)).unwrap_err();
        assert_eq!(
            err,
            mostro_core::error::MostroError::MostroCantDo(CantDoReason::InvalidTextMessage)
        );
    }

    #[test]
    fn parse_solver_payload_rejects_empty_and_whitespace_only_text() {
        for raw in ["", "   "] {
            let err = parse_solver_payload(&Payload::TextMessage(raw.to_string())).unwrap_err();
            assert_eq!(
                err,
                mostro_core::error::MostroError::MostroCantDo(CantDoReason::InvalidTextMessage)
            );
        }
    }

    #[test]
    fn parse_solver_payload_rejects_empty_npub_with_permission() {
        let err = parse_solver_payload(&Payload::TextMessage(":read".to_string())).unwrap_err();
        assert_eq!(
            err,
            mostro_core::error::MostroError::MostroCantDo(CantDoReason::InvalidTextMessage)
        );
    }

    #[test]
    fn parse_solver_payload_rejects_extra_tokens() {
        let err = parse_solver_payload(&Payload::TextMessage("npub1test:read:extra".to_string()))
            .unwrap_err();
        assert_eq!(
            err,
            mostro_core::error::MostroError::MostroCantDo(CantDoReason::InvalidParameters)
        );
    }

    mod action {
        use super::super::admin_add_solver_action;
        use super::{SOLVER_CATEGORY_READ_ONLY, SOLVER_CATEGORY_READ_WRITE};
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        use crate::app::context::AppContext;
        use crate::db::is_user_present;
        use mostro_core::prelude::*;
        use nostr_sdk::prelude::*;
        use sqlx::SqlitePool;
        use std::sync::Arc;

        async fn create_test_pool() -> SqlitePool {
            let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
            sqlx::migrate!().run(&pool).await.unwrap();
            pool
        }

        fn build_ctx(pool: &SqlitePool) -> AppContext {
            // send_dm reads the global config (event expiration); seed it
            // once, ignoring the error when another test already did.
            let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
            TestContextBuilder::new()
                .with_pool(Arc::new(pool.clone()))
                .with_settings(test_settings())
                .build()
        }

        fn add_solver_msg(payload: Option<Payload>) -> Message {
            Message::new_dispute(None, Some(1), None, Action::AdminAddSolver, payload)
        }

        /// Event whose identity is `identity` — the handler only accepts the
        /// admin's own identity key.
        fn create_event(identity: PublicKey) -> UnwrappedMessage {
            UnwrappedMessage {
                message: add_solver_msg(None),
                signature: None,
                sender: identity,
                identity,
                created_at: Timestamp::now(),
            }
        }

        #[tokio::test]
        async fn rejects_message_without_payload() {
            let pool = create_test_pool().await;
            let ctx = build_ctx(&pool);
            let my_keys = Keys::generate();
            let event = create_event(my_keys.public_key());

            let result =
                admin_add_solver_action(&ctx, add_solver_msg(None), &event, &my_keys).await;

            assert!(matches!(
                result,
                Err(MostroCantDo(CantDoReason::InvalidTextMessage))
            ));
        }

        #[tokio::test]
        async fn rejects_non_admin_identity() {
            let pool = create_test_pool().await;
            let ctx = build_ctx(&pool);
            let my_keys = Keys::generate();
            // Identity does NOT match my_keys
            let event = create_event(Keys::generate().public_key());
            let npub = Keys::generate().public_key().to_bech32().unwrap();
            let msg = add_solver_msg(Some(Payload::TextMessage(npub)));

            let result = admin_add_solver_action(&ctx, msg, &event, &my_keys).await;

            assert!(matches!(
                result,
                Err(MostroInternalErr(ServiceError::InvalidPubkey))
            ));
        }

        #[tokio::test]
        async fn rejects_invalid_npub_string() {
            let pool = create_test_pool().await;
            let ctx = build_ctx(&pool);
            let my_keys = Keys::generate();
            let event = create_event(my_keys.public_key());
            let msg = add_solver_msg(Some(Payload::TextMessage("not-an-npub".to_string())));

            let result = admin_add_solver_action(&ctx, msg, &event, &my_keys).await;

            assert!(matches!(
                result,
                Err(MostroInternalErr(ServiceError::InvalidPubkey))
            ));
        }

        /// Happy path: solver is inserted with the parsed category and the
        /// confirmation DM succeeds offline (no global Nostr client set).
        #[tokio::test]
        async fn adds_solver_with_read_only_category() {
            let pool = create_test_pool().await;
            let ctx = build_ctx(&pool);
            let my_keys = Keys::generate();
            let event = create_event(my_keys.public_key());
            let solver_pubkey = Keys::generate().public_key();
            let npub = solver_pubkey.to_bech32().unwrap();
            let msg = add_solver_msg(Some(Payload::TextMessage(format!("{npub}:read"))));

            let result = admin_add_solver_action(&ctx, msg, &event, &my_keys).await;

            assert!(result.is_ok());
            let user = is_user_present(&pool, solver_pubkey.to_string())
                .await
                .unwrap();
            assert_eq!(user.is_solver, 1);
            assert_eq!(user.category, SOLVER_CATEGORY_READ_ONLY);
        }

        #[tokio::test]
        async fn adds_solver_with_default_read_write_category() {
            let pool = create_test_pool().await;
            let ctx = build_ctx(&pool);
            let my_keys = Keys::generate();
            let event = create_event(my_keys.public_key());
            let solver_pubkey = Keys::generate().public_key();
            let npub = solver_pubkey.to_bech32().unwrap();
            let msg = add_solver_msg(Some(Payload::TextMessage(npub)));

            let result = admin_add_solver_action(&ctx, msg, &event, &my_keys).await;

            assert!(result.is_ok());
            let user = is_user_present(&pool, solver_pubkey.to_string())
                .await
                .unwrap();
            assert_eq!(user.category, SOLVER_CATEGORY_READ_WRITE);
        }

        #[tokio::test]
        async fn rejects_duplicate_solver_insert() {
            let pool = create_test_pool().await;
            let ctx = build_ctx(&pool);
            let my_keys = Keys::generate();
            let event = create_event(my_keys.public_key());
            let npub = Keys::generate().public_key().to_bech32().unwrap();

            let first = admin_add_solver_action(
                &ctx,
                add_solver_msg(Some(Payload::TextMessage(npub.clone()))),
                &event,
                &my_keys,
            )
            .await;
            assert!(first.is_ok());

            let second = admin_add_solver_action(
                &ctx,
                add_solver_msg(Some(Payload::TextMessage(npub))),
                &event,
                &my_keys,
            )
            .await;

            assert!(matches!(
                second,
                Err(MostroInternalErr(ServiceError::DbAccessError(_)))
            ));
        }
    }
}
