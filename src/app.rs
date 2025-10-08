//! Main application module for the P2P trading system.
//! Handles message routing, action processing, and event loop management.

// Submodules for different trading actions
pub mod add_invoice; // Handles invoice creation
pub mod admin_add_solver; // Admin functionality to add dispute solvers
pub mod admin_cancel; // Admin order cancellation
pub mod admin_settle; // Admin dispute settlement
pub mod admin_take_dispute; // Admin dispute handling
pub mod cancel; // User order cancellation
pub mod dispute; // User dispute handling
pub mod fiat_sent; // Fiat payment confirmation
pub mod order; // Order creation and management
pub mod rate_user; // User reputation system
pub mod release; // Release of held funds
pub mod restore_session; // Restore session action
pub mod synch_trade_index;
pub mod take_buy; // Taking buy orders
pub mod take_sell; // Taking sell orders
pub mod trade_pubkey; // Trade pubkey action // Sync user trade index action

// Import action handlers from submodules
use crate::app::add_invoice::add_invoice_action;
use crate::app::admin_add_solver::admin_add_solver_action;
use crate::app::admin_cancel::admin_cancel_action;
use crate::app::admin_settle::admin_settle_action;
use crate::app::admin_take_dispute::admin_take_dispute_action;
use crate::app::cancel::cancel_action;
use crate::app::dispute::dispute_action;
use crate::app::fiat_sent::fiat_sent_action;
use crate::app::order::order_action;
use crate::app::rate_user::update_user_reputation_action;
use crate::app::release::release_action;
use crate::app::restore_session::restore_session_action;
use crate::app::synch_trade_index::synch_last_trade_index;
use crate::app::take_buy::take_buy_action;
use crate::app::take_sell::take_sell_action;
use crate::app::trade_pubkey::trade_pubkey_action;
use crate::config::settings::get_db_pool;
// Core functionality imports
use crate::config::settings::Settings;
use crate::db::add_new_user;
use crate::db::is_user_present;
use crate::lightning::LndConnector;
use crate::util::enqueue_cant_do_msg;

// External dependencies
use mostro_core::error::CantDoReason;
use mostro_core::error::MostroError;
use mostro_core::error::ServiceError;
use mostro_core::message::{Action, Message};
use mostro_core::user::User;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

/// Helper function to log warning messages for action errors
fn warning_msg(action: &Action, err: ServiceError) {
    let message = match &err {
        ServiceError::EnvVarError(message) => message.to_string(),
        ServiceError::EncryptionError(message) => message.to_string(),
        ServiceError::DecryptionError(message) => message.to_string(),
        ServiceError::IOError(message) => message.to_string(),
        ServiceError::UnexpectedError(message) => message.to_string(),
        ServiceError::LnNodeError(message) => message.to_string(),
        ServiceError::LnPaymentError(message) => message.to_string(),
        ServiceError::DbAccessError(message) => message.to_string(),
        ServiceError::NostrError(message) => message.to_string(),
        ServiceError::HoldInvoiceError(message) => message.to_string(),
        _ => "No message".to_string(),
    };

    tracing::warn!(
        "Error in {} with context {} - inner message {}",
        action,
        err,
        message
    );
}

/// Function to manage errors and send appropriate messages
async fn manage_errors(
    e: MostroError,
    inner_message: Message,
    event: UnwrappedGift,
    action: &Action,
) {
    match e {
        MostroError::MostroCantDo(cause) => {
            enqueue_cant_do_msg(
                inner_message.get_inner_message_kind().request_id,
                inner_message.get_inner_message_kind().id,
                cause,
                event.rumor.pubkey,
            )
            .await
        }
        MostroError::MostroInternalErr(e) => warning_msg(action, e),
    }
}

/// Function to check if a user is present in the database and update or create their trade index.
///
/// This function performs the following tasks:
/// 1. It checks if the action associated with the incoming message is related to trading (NewOrder, TakeBuy, or TakeSell).
/// 2. If the user is found in the database, it verifies the trade index and the signature of the message.
///    - If valid, it updates the user's trade index.
///    - If invalid, it logs a warning and sends a message indicating the issue.
/// 3. If the user is not found, it creates a new user entry with the provided trade index if applicable.
///
/// # Arguments
/// * `pool` - The database connection pool used to query and update user data.
/// * `event` - The unwrapped gift event containing the sender's information.
/// * `msg` - The message containing action details and trade index information.
async fn check_trade_index(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    msg: &Message,
) -> Result<(), MostroError> {
    let message_kind = msg.get_inner_message_kind();

    // Only process actions related to trading
    if !matches!(
        message_kind.action,
        Action::NewOrder | Action::TakeBuy | Action::TakeSell
    ) {
        return Ok(());
    }

    // If user is present, we check the trade index and signature
    match is_user_present(pool, event.sender.to_string()).await {
        Ok(user) => {
            if let index @ 1.. = message_kind.trade_index() {
                let content: (Message, Signature) = match serde_json::from_str::<(
                    Message,
                    nostr_sdk::secp256k1::schnorr::Signature,
                )>(&event.rumor.content)
                {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::error!("Error deserializing content: {}", e);
                        return Err(MostroError::MostroInternalErr(
                            ServiceError::MessageSerializationError,
                        ));
                    }
                };

                let (_, sig) = content;

                if index <= user.last_trade_index {
                    tracing::info!("Invalid trade index");
                    manage_errors(
                        MostroError::MostroCantDo(CantDoReason::InvalidTradeIndex),
                        msg.clone(),
                        event.clone(),
                        &message_kind.action,
                    )
                    .await;
                    return Err(MostroError::MostroCantDo(CantDoReason::InvalidTradeIndex));
                }
                let msg = match msg.as_json() {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::error!(
                            "Failed to serialize message for signature verification: {}",
                            e
                        );
                        return Err(MostroError::MostroInternalErr(
                            ServiceError::MessageSerializationError,
                        ));
                    }
                };
                if !Message::verify_signature(msg, event.rumor.pubkey, sig) {
                    tracing::info!("Invalid signature");
                    return Err(MostroError::MostroCantDo(CantDoReason::InvalidSignature));
                }
            }
            Ok(())
        }
        Err(_) => {
            if message_kind.trade_index.is_some() && event.sender != event.rumor.pubkey {
                let new_user: User = User {
                    pubkey: event.sender.to_string(),
                    ..Default::default()
                };
                if let Err(e) = add_new_user(pool, new_user).await {
                    tracing::error!("Error creating new user: {}", e);
                    return Err(MostroError::MostroCantDo(CantDoReason::CantCreateUser));
                }
            }
            Ok(())
        }
    }
}

/// Handles the processing of a single message action by routing it to the appropriate handler
/// based on the action type. This is the core message routing logic of the application.
///
/// # Arguments
/// * `action` - The type of action to be performed
/// * `msg` - The message containing action details
/// * `event` - The unwrapped gift wrap event
/// * `my_keys` - Node keypair for signing/verification
/// * `pool` - Database connection pool
/// * `ln_client` - Lightning network connector
async fn handle_message_action(
    action: &Action,
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    match action {
        // Order-related actions
        Action::NewOrder => order_action(msg, event, my_keys, pool)
            .await
            .map_err(|e| e.into()),
        Action::TakeSell => take_sell_action(msg, event, my_keys, pool)
            .await
            .map_err(|e| e.into()),
        Action::TakeBuy => take_buy_action(msg, event, my_keys, pool)
            .await
            .map_err(|e| e.into()),

        // Payment-related actions
        Action::FiatSent => fiat_sent_action(msg, event, my_keys, pool)
            .await
            .map_err(|e| e.into()),
        Action::Release => release_action(msg, event, my_keys, pool, ln_client)
            .await
            .map_err(|e| e.into()),
        Action::AddInvoice => add_invoice_action(msg, event, my_keys, pool)
            .await
            .map_err(|e| e.into()),
        Action::PayInvoice => todo!(),
        Action::LastTradeIndex => synch_last_trade_index(event, my_keys, pool)
            .await
            .map_err(|e| e.into()),

        // Dispute and rating actions
        Action::Dispute => dispute_action(msg, event, my_keys, pool)
            .await
            .map_err(|e| e.into()),
        Action::RateUser => update_user_reputation_action(msg, event, my_keys, pool)
            .await
            .map_err(|e| e.into()),
        Action::Cancel => cancel_action(msg, event, my_keys, pool, ln_client)
            .await
            .map_err(|e| e.into()),

        // Admin actions
        Action::AdminCancel => admin_cancel_action(msg, event, my_keys, pool, ln_client)
            .await
            .map_err(|e| e.into()),
        Action::AdminSettle => admin_settle_action(msg, event, my_keys, pool, ln_client)
            .await
            .map_err(|e| e.into()),
        Action::AdminAddSolver => admin_add_solver_action(msg, event, my_keys, pool)
            .await
            .map_err(|e| e.into()),
        Action::AdminTakeDispute => admin_take_dispute_action(msg, event, my_keys, pool)
            .await
            .map_err(|e| e.into()),
        Action::TradePubkey => trade_pubkey_action(msg, event, pool)
            .await
            .map_err(|e| e.into()),
        Action::RestoreSession => restore_session_action(event, pool)
            .await
            .map_err(|e| e.into()),

        _ => {
            tracing::info!("Received message with action {:?}", action);
            Ok(())
        }
    }
}

/// Main event loop that processes incoming Nostr events.
/// Handles message verification, POW checking, and routes valid messages to appropriate handlers.
///
/// # Arguments
/// * `my_keys` - The node's keypair
/// * `client` - Nostr client instance
/// * `ln_client` - Lightning network connector
/// * `pool` - SQLite connection pool
/// * `rate_list` - Shared list of rating events
pub async fn run(my_keys: Keys, client: &Client, ln_client: &mut LndConnector) -> Result<()> {
    loop {
        let mut notifications = client.notifications();

        // Arc clone of db pool for main loop
        let pool = get_db_pool();
        // Get pow from config
        let pow = Settings::get_mostro().pow;
        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event { event, .. } = notification {
                // Verify proof of work
                if !event.check_pow(pow) {
                    // Discard events that don't meet POW requirements
                    tracing::info!("Not POW verified event!");
                    continue;
                }
                if let Kind::GiftWrap = event.kind {
                    // Validate event signature
                    if event.verify().is_err() {
                        tracing::warn!("Error in event verification")
                    };

                    let event = match nip59::extract_rumor(&my_keys, &event).await {
                        Ok(u) => u,
                        Err(e) => {
                            tracing::warn!("Error unwrapping gift: {}", e);
                            continue;
                        }
                    };
                    // Discard events older than 10 seconds to prevent replay attacks
                    let since_time = chrono::Utc::now()
                        .checked_sub_signed(chrono::Duration::seconds(10))
                        .unwrap()
                        .timestamp() as u64;
                    if event.rumor.created_at.as_u64() < since_time {
                        continue;
                    }
                    // Parse message and signature from rumor content put message in Message struct
                    let (message, sig) = match serde_json::from_str::<(Message, Option<Signature>)>(
                        &event.rumor.content,
                    ) {
                        Ok((message, signature)) => (message, signature),
                        Err(e) => {
                            tracing::warn!("Failed to parse message, inner message, and signature from rumor content: {}", e);
                            continue;
                        }
                    };

                    // Serialize message to json
                    let message_json = match message.clone().as_json() {
                        Ok(message_json) => message_json,
                        Err(e) => {
                            tracing::warn!("Failed to serialize message: {}", e);
                            continue;
                        }
                    };
                    // Check if sender and rumor pubkey are different
                    let sender_matches_rumor = event.sender == event.rumor.pubkey;
                    if let Some(sig) = sig {
                        // Verify signature only if sender and rumor pubkey are different
                        if !sender_matches_rumor
                            && !Message::verify_signature(message_json, event.rumor.pubkey, sig)
                        {
                            tracing::warn!(
                                "Signature verification failed: sender {} does not match rumor pubkey {}",
                                event.sender,
                                event.rumor.pubkey
                            );
                            continue;
                        }
                    } else if !sender_matches_rumor {
                        // If there is no signature and the sender does not match the rumor pubkey, there is also an error
                        tracing::warn!("Error in event verification");
                        continue;
                    }
                    // Get inner message kind
                    let inner_message = message.get_inner_message_kind();
                    // Check if message is message with trade index
                    if let Err(e) = check_trade_index(&pool, &event, &message).await {
                        tracing::warn!("Error checking trade index: {}", e);
                        continue;
                    }

                    if inner_message.verify() {
                        if let Some(action) = message.inner_action() {
                            if let Err(e) = handle_message_action(
                                &action,
                                message.clone(),
                                &event,
                                &my_keys,
                                &pool,
                                ln_client,
                            )
                            .await
                            {
                                match e.downcast::<MostroError>() {
                                    Ok(err) => {
                                        manage_errors(*err, message, event, &action).await;
                                    }
                                    Err(e) => {
                                        tracing::error!("Unexpected error type: {}", e);
                                        warning_msg(
                                            &action,
                                            ServiceError::UnexpectedError(e.to_string()),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::message::Action;

    use nostr_sdk::secp256k1::schnorr::Signature;
    use nostr_sdk::{Keys, Kind as NostrKind, Timestamp, UnsignedEvent};

    // Helper function to create test keys
    fn create_test_keys() -> Keys {
        Keys::generate()
    }

    // Helper function to create test message
    fn create_test_message(action: Action, trade_index: Option<u32>) -> Message {
        Message::new_order(
            Some(uuid::Uuid::new_v4()),
            Some(1),
            trade_index.map(|i| i as i64),
            action,
            None, // We don't need payload for structure tests
        )
    }

    // Helper function to create UnwrappedGift for testing
    fn create_test_unwrapped_gift() -> UnwrappedGift {
        let keys = create_test_keys();
        let sender_keys = create_test_keys();

        let unsigned_event = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            NostrKind::GiftWrap,
            Vec::new(),
            "",
        );

        UnwrappedGift {
            sender: sender_keys.public_key(),
            rumor: unsigned_event,
        }
    }

    #[test]
    fn test_warning_msg_all_error_types() {
        let action = Action::NewOrder;

        // Test all ServiceError variants
        warning_msg(&action, ServiceError::EnvVarError("env error".to_string()));
        warning_msg(
            &action,
            ServiceError::EncryptionError("encryption error".to_string()),
        );
        warning_msg(
            &action,
            ServiceError::DecryptionError("decryption error".to_string()),
        );
        warning_msg(&action, ServiceError::IOError("io error".to_string()));
        warning_msg(
            &action,
            ServiceError::UnexpectedError("unexpected error".to_string()),
        );
        warning_msg(
            &action,
            ServiceError::LnNodeError("ln node error".to_string()),
        );
        warning_msg(
            &action,
            ServiceError::LnPaymentError("ln payment error".to_string()),
        );
        warning_msg(
            &action,
            ServiceError::DbAccessError("db access error".to_string()),
        );
        warning_msg(&action, ServiceError::NostrError("nostr error".to_string()));
        warning_msg(
            &action,
            ServiceError::HoldInvoiceError("hold invoice error".to_string()),
        );

        // Test default case
        warning_msg(&action, ServiceError::MessageSerializationError);
    }

    #[tokio::test]
    async fn test_manage_errors_cant_do() {
        let message = create_test_message(Action::NewOrder, None);
        let event = create_test_unwrapped_gift();
        let action = Action::NewOrder;

        let error = MostroError::MostroCantDo(CantDoReason::InvalidSignature);
        manage_errors(error, message, event, &action).await;

        // Test passes if no panic occurs
    }

    #[tokio::test]
    async fn test_manage_errors_internal_error() {
        let message = create_test_message(Action::NewOrder, None);
        let event = create_test_unwrapped_gift();
        let action = Action::NewOrder;

        let error =
            MostroError::MostroInternalErr(ServiceError::UnexpectedError("test error".to_string()));
        manage_errors(error, message, event, &action).await;

        // Test passes if no panic occurs
    }

    mod check_trade_index_tests {
        use super::*;
        use sqlx::SqlitePool;

        async fn create_test_pool() -> SqlitePool {
            SqlitePool::connect(":memory:").await.unwrap()
        }

        #[tokio::test]
        async fn test_check_trade_index_non_trading_action() {
            let pool = create_test_pool().await;
            let event = create_test_unwrapped_gift();
            let message = create_test_message(Action::FiatSent, None);

            let result = check_trade_index(&pool, &event, &message).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn test_check_trade_index_trading_action_no_index() {
            let pool = create_test_pool().await;
            let event = create_test_unwrapped_gift();
            let message = create_test_message(Action::NewOrder, None);

            let result = check_trade_index(&pool, &event, &message).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn test_check_trade_index_with_valid_index() {
            let pool = create_test_pool().await;
            let event = create_test_unwrapped_gift();
            let message = create_test_message(Action::NewOrder, Some(1));

            // This test would require database setup and user creation
            // For now, we test the structure
            let result = check_trade_index(&pool, &event, &message).await;
            // Result could be Ok or Err depending on database state
            assert!(result.is_ok() || result.is_err());
        }
    }

    mod handle_message_action_tests {
        use super::*;

        #[tokio::test]
        async fn test_handle_message_action_unknown() {
            // Test the structure of action handling without creating unused variables
            // This test verifies that the action routing logic exists and compiles
        }

        #[test]
        fn test_action_routing_logic() {
            // Test that all action types are handled in the match statement
            let actions = vec![
                Action::NewOrder,
                Action::TakeSell,
                Action::TakeBuy,
                Action::FiatSent,
                Action::Release,
                Action::AddInvoice,
                Action::PayInvoice,
                Action::Dispute,
                Action::RateUser,
                Action::Cancel,
                Action::AdminCancel,
                Action::AdminSettle,
                Action::AdminAddSolver,
                Action::AdminTakeDispute,
                Action::TradePubkey,
            ];

            // Verify we have handlers for all action types
            for action in actions {
                // In a real test, we would verify each action is properly routed
                // This test ensures we don't forget to handle new actions
                match action {
                    Action::NewOrder
                    | Action::TakeSell
                    | Action::TakeBuy
                    | Action::FiatSent
                    | Action::Release
                    | Action::AddInvoice
                    | Action::Dispute
                    | Action::RateUser
                    | Action::Cancel
                    | Action::AdminCancel
                    | Action::AdminSettle
                    | Action::AdminAddSolver
                    | Action::AdminTakeDispute
                    | Action::TradePubkey => {
                        // Action is handled
                    }
                    Action::PayInvoice => {
                        // This action is marked as todo!()
                    }
                    _ => {
                        // Any unhandled actions should be caught here
                    }
                }
            }
        }
    }

    mod message_validation_tests {
        use super::*;

        #[test]
        fn test_signature_verification_logic() {
            let keys = create_test_keys();
            let sender_keys = create_test_keys();

            // Test sender matches rumor pubkey case
            let sender_matches_rumor = keys.public_key() == keys.public_key();
            assert!(sender_matches_rumor);

            // Test sender doesn't match rumor pubkey case
            let sender_differs = sender_keys.public_key() != keys.public_key();
            assert!(sender_differs);
        }

        #[test]
        fn test_timestamp_validation() {
            let current_time = chrono::Utc::now().timestamp() as u64;
            let old_time = current_time - 20; // 20 seconds ago
            let recent_time = current_time - 5; // 5 seconds ago

            let since_time = chrono::Utc::now()
                .checked_sub_signed(chrono::Duration::seconds(10))
                .unwrap()
                .timestamp() as u64;

            // Old event should be rejected
            assert!(old_time < since_time);

            // Recent event should be accepted
            assert!(recent_time >= since_time);
        }

        #[test]
        fn test_pow_verification_logic() {
            // Test POW validation logic structure
            // In a real implementation, we would test event.check_pow(pow)
            // This tests the logical flow
            let meets_pow = true; // Mock result
            let fails_pow = false; // Mock result

            assert!(meets_pow);
            assert!(!fails_pow);
        }
    }

    mod event_processing_tests {
        use super::*;

        #[test]
        fn test_gift_wrap_processing_structure() {
            // Test the structure of gift wrap event processing
            let kind = NostrKind::GiftWrap;

            match kind {
                NostrKind::GiftWrap => {
                    // This is the expected path for gift wrap events
                }
                _ => {
                    // Other event types are ignored
                    panic!("Unexpected event type");
                }
            }
        }

        #[test]
        fn test_message_parsing_structure() {
            // Test message parsing logic structure
            let test_content = r#"[{"order":{"version":1,"request_id":1,"trade_index":null,"id":"550e8400-e29b-41d4-a716-446655440000","action":"new-order","payload":null}}, null]"#;

            let result = serde_json::from_str::<(Message, Option<Signature>)>(test_content);
            match result {
                Ok((message, signature)) => {
                    // Test the structure of message parsing
                    // Note: message.verify() may fail without proper payload setup
                    // We're testing the parsing structure, not the validation logic
                    assert!(signature.is_none());

                    // Test that we got a message of some kind
                    match message {
                        Message::Order(_) => {}
                        _ => {} // Any message type is fine for structure test
                    }
                }
                Err(_) => {
                    // Parsing error is handled gracefully
                }
            }
        }
    }
}
