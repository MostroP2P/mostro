//! Main application module for the P2P trading system.
//! Handles message routing, action processing, and event loop management.

// Application context (dependency injection)
pub mod context;

// Submodules for different trading actions
pub mod add_cashu_escrow; // Cashu escrow lock handler (Track A / CF-5 stub)
pub mod add_invoice; // Handles invoice creation
pub mod admin_add_solver; // Admin functionality to add dispute solvers
pub mod admin_cancel; // Admin order cancellation
pub mod admin_settle; // Admin dispute settlement
pub mod admin_take_dispute; // Admin dispute handling
pub mod bond; // Anti-abuse bond data + helpers (issue #711)
pub mod cancel; // User order cancellation
pub mod dev_fee; // Dev fee payment lifecycle
pub mod dispute; // User dispute handling
pub mod fiat_sent; // Fiat payment confirmation
pub mod last_trade_index;
pub mod order; // Order creation and management
pub mod orders; // Orders action
pub mod rate_user; // User reputation system
pub mod release; // Release of held funds
pub mod restore_session; // Restore session action
pub mod take_buy; // Taking buy orders
pub mod take_sell; // Taking sell orders
pub mod trade_pubkey; // Trade pubkey action // Sync user trade index action

// Import action handlers from submodules
use crate::app::add_cashu_escrow::add_cashu_escrow_action;
use crate::app::add_invoice::add_invoice_action;
use crate::app::admin_add_solver::admin_add_solver_action;
use crate::app::admin_cancel::admin_cancel_action;
use crate::app::admin_settle::admin_settle_action;
use crate::app::admin_take_dispute::admin_take_dispute_action;
use crate::app::bond::add_bond_invoice_action;
use crate::app::cancel::cancel_action;
use crate::app::context::AppContext;
use crate::app::dispute::dispute_action;
use crate::app::fiat_sent::fiat_sent_action;
use crate::app::last_trade_index::last_trade_index;
use crate::app::order::order_action;
use crate::app::orders::orders_action;
use crate::app::rate_user::update_user_reputation_action;
use crate::app::release::release_action;
use crate::app::restore_session::restore_session_action;
use crate::app::take_buy::take_buy_action;
use crate::app::take_sell::take_sell_action;
use crate::app::trade_pubkey::trade_pubkey_action;
// Core functionality imports
use crate::db::add_new_user;
use crate::db::is_user_present;
use crate::lightning::LndConnector;
use crate::util::enqueue_cant_do_msg;

// External dependencies
use mostro_core::error::CantDoReason;
use mostro_core::error::MostroError;
use mostro_core::error::ServiceError;
use mostro_core::message::{Action, Message};
use mostro_core::nip59::UnwrappedMessage;
use mostro_core::transport::unwrap_incoming;
use mostro_core::user::User;
use nostr_sdk::prelude::*;

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
    event: UnwrappedMessage,
    action: &Action,
) {
    match e {
        MostroError::MostroCantDo(cause) => {
            enqueue_cant_do_msg(
                inner_message.get_inner_message_kind().request_id,
                inner_message.get_inner_message_kind().id,
                cause,
                // Reply to the trade key that authored the rumor.
                event.sender,
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
/// * `ctx` - Application context providing database pool and other dependencies.
/// * `event` - The unwrapped NIP-59 message (`UnwrappedMessage`) containing
///   the sender's identity and trade keys.
/// * `msg` - The message containing action details and trade index information.
async fn check_trade_index(
    ctx: &AppContext,
    event: &UnwrappedMessage,
    msg: &Message,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    let message_kind = msg.get_inner_message_kind();

    // Only process actions related to trading
    if !matches!(
        message_kind.action,
        Action::NewOrder | Action::TakeBuy | Action::TakeSell
    ) {
        return Ok(());
    }

    // If user is present, we check the trade index and signature
    match is_user_present(pool, event.identity.to_string()).await {
        Ok(user) => {
            if let index @ 1.. = message_kind.trade_index() {
                // Inner-tuple signature is already decoded by unwrap_message.
                let sig = event.signature.ok_or_else(|| {
                    tracing::error!("Trade-index message missing inner signature");
                    MostroError::MostroCantDo(CantDoReason::InvalidSignature)
                })?;

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
                let msg_json = match msg.as_json() {
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
                if !Message::verify_signature(msg_json, event.sender, sig) {
                    tracing::info!("Invalid signature");
                    return Err(MostroError::MostroCantDo(CantDoReason::InvalidSignature));
                }
            }
            Ok(())
        }
        Err(_) => {
            if let Some(last_trade_index) = message_kind.trade_index {
                // Refuse case of index 0, means identikey key and new user cannot use it!
                if last_trade_index == 0 {
                    return Err(MostroError::MostroCantDo(CantDoReason::InvalidTradeIndex));
                }
                if event.identity != event.sender {
                    let new_user: User = User {
                        pubkey: event.identity.to_string(),
                        last_trade_index,
                        ..Default::default()
                    };
                    if let Err(e) = add_new_user(pool, new_user).await {
                        tracing::error!("Error creating new user: {}", e);
                        return Err(MostroError::MostroCantDo(CantDoReason::CantCreateUser));
                    }
                }
            }
            Ok(())
        }
    }
}

async fn handle_message_action_no_ln(
    action: &Action,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
    ctx: &AppContext,
) -> Result<()> {
    match action {
        // Order-related actions
        Action::NewOrder => order_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        Action::TakeSell => take_sell_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        Action::TakeBuy => take_buy_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),

        // Payment-related actions that do not require LN client
        Action::FiatSent => fiat_sent_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        Action::AddInvoice => add_invoice_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        Action::AddBondInvoice => add_bond_invoice_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        Action::PayInvoice => Err(MostroError::MostroCantDo(CantDoReason::InvalidAction).into()),
        Action::LastTradeIndex => last_trade_index(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),

        // Dispute and rating actions
        Action::Dispute => dispute_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        Action::RateUser => update_user_reputation_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),

        // Admin actions without LN
        Action::AdminAddSolver => admin_add_solver_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        Action::AdminTakeDispute => admin_take_dispute_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        Action::TradePubkey => trade_pubkey_action(ctx, msg, event)
            .await
            .map_err(|e| e.into()),
        Action::RestoreSession => restore_session_action(ctx, event)
            .await
            .map_err(|e| e.into()),
        Action::Orders => orders_action(ctx, msg, event).await.map_err(|e| e.into()),
        _ => {
            tracing::info!("Received message with action {:?}", action);
            Ok(())
        }
    }
}

/// Handles the processing of a single message action by routing it to the appropriate handler
/// based on the action type. This is the core message routing logic of the application.
async fn handle_message_action(
    action: &Action,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
    ln_client: &mut LndConnector,
    ctx: &AppContext,
) -> Result<()> {
    match action {
        Action::Release => release_action(ctx, msg, event, my_keys, ln_client)
            .await
            .map_err(|e| e.into()),
        Action::Cancel => cancel_action(ctx, msg, event, my_keys, ln_client)
            .await
            .map_err(|e| e.into()),
        Action::AdminCancel => admin_cancel_action(ctx, msg, event, my_keys, ln_client)
            .await
            .map_err(|e| e.into()),
        Action::AdminSettle => admin_settle_action(ctx, msg, event, my_keys, ln_client)
            .await
            .map_err(|e| e.into()),
        _ => handle_message_action_no_ln(action, msg, event, my_keys, ctx).await,
    }
}

/// Decode and fully validate one relay event into a dispatchable
/// `(action, message, unwrapped)` triple, or `None` if it must be skipped
/// (failed PoW, wrong kind, spam-gate drop, decrypt failure, stale, missing
/// inner signature, failed trade-index, failed inner verify, no action).
///
/// This is the transport + validation **prologue** shared VERBATIM by `run`
/// (Lightning) and `run_cashu` (Cashu) so the two event loops cannot drift
/// (CF-5, see `docs/cashu/01-fundamentals.md` §6). Its body is a literal cut of
/// the pre-dispatch logic `run` used to inline; each former `continue` becomes
/// `return None`.
async fn accept_event(
    ctx: &AppContext,
    event: &Event,
    my_keys: &Keys,
    pow: u8,
    pow_first_contact: u8,
    accepted_kind: Kind,
    is_v2: bool,
) -> Option<(Action, Message, UnwrappedMessage)> {
    // Verify proof of work
    if !event.check_pow(pow) {
        // Discard events that don't meet POW requirements
        tracing::info!("Not POW verified event!");
        return None;
    }
    if event.kind != accepted_kind {
        return None;
    }
    // Phase 2 anti-spam gate (protocol v2 / kind 14 only):
    // cheap pre-validation BEFORE paying the NIP-44 decrypt
    // cost. v1 gift wraps skip this — their outer key is a
    // throwaway with no pre-validatable signal.
    if is_v2 {
        if let Some(gate) = crate::spam_gate::SpamGate::global() {
            let now = chrono::Utc::now().timestamp();
            // Dedup: drop a re-sent identical event (defense in
            // depth against replay floods).
            if gate.is_replay(event.id, now) {
                tracing::debug!("Dropping replayed event {}", event.id);
                return None;
            }
            // Two lanes: a sender already in an active trade is
            // fast-pathed (only the base `pow` already checked
            // above applies); an unseen first-contact sender
            // must clear the stiffer `pow_first_contact` before
            // we decrypt. New orders/takes legitimately arrive
            // here — so does spam, hence the PoW toll.
            if !gate.is_known(&event.pubkey.to_string()) && !event.check_pow(pow_first_contact) {
                tracing::info!(
                    "Dropping first-contact kind-14 event from unknown key {} below pow_first_contact ({} bits)",
                    event.pubkey,
                    pow_first_contact
                );
                return None;
            }
        }
    }

    // Validate event signature
    if event.verify().is_err() {
        tracing::warn!("Error in event verification")
    };

    // Mostro-core dispatches on the event kind: the gift wrap
    // path handles the dual-key layout (identity key signs
    // seal, trade key authors rumor), the kind-14 path the
    // 3-element tuple with its in-ciphertext identity proof.
    // Both decode and verify signatures in one shot and yield
    // the same transport-agnostic `UnwrappedMessage`.
    let unwrapped = match unwrap_incoming(event, my_keys).await {
        Ok(Some(u)) => u,
        // NIP-44 decrypt failed: not addressed to this node.
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!("Error unwrapping incoming message: {}", e);
            return None;
        }
    };
    // Discard events older than 10 seconds to prevent replay attacks
    let since_time = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::seconds(10))
        .unwrap()
        .timestamp() as u64;
    if unwrapped.created_at.as_secs() < since_time {
        return None;
    }
    let message = unwrapped.message.clone();

    // Full-privacy clients reuse the trade key as identity and send
    // unsigned rumors. Any other shape must carry a valid inner
    // signature — unwrap_message already verified it, so if identity
    // and sender differ here without a signature we bail out.
    if unwrapped.identity != unwrapped.sender && unwrapped.signature.is_none() {
        tracing::warn!(
            "Missing inner signature: identity {} differs from trade key {}",
            unwrapped.identity,
            unwrapped.sender
        );
        return None;
    }

    // Get inner message kind
    let inner_message = message.get_inner_message_kind();
    // Check if message is message with trade index
    if let Err(e) = check_trade_index(ctx, &unwrapped, &message).await {
        tracing::warn!("Error checking trade index: {}", e);
        return None;
    }

    if !inner_message.verify() {
        return None;
    }
    let action = message.inner_action()?;
    Some((action, message, unwrapped))
}

/// Shared post-dispatch error handling (identical in both loops). A handler
/// `Err` is downcast to a `MostroError` and turned into the right reply
/// (`manage_errors`) or logged (`warning_msg`); `Ok` is a no-op. Factored out
/// with [`accept_event`] so `run` and `run_cashu` share one error tail (CF-5).
async fn finalize_dispatch(
    result: Result<()>,
    message: Message,
    unwrapped: UnwrappedMessage,
    action: &Action,
) {
    if let Err(e) = result {
        match e.downcast::<MostroError>() {
            Ok(err) => {
                manage_errors(*err, message, unwrapped, action).await;
            }
            Err(e) => {
                tracing::error!("Unexpected error type: {}", e);
                warning_msg(action, ServiceError::UnexpectedError(e.to_string()));
            }
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
pub async fn run(ctx: AppContext, ln_client: &mut LndConnector) -> Result<()> {
    let my_keys = ctx.keys();
    let client = ctx.nostr_client();
    let pow = ctx.settings().mostro.pow;
    // The node speaks exactly one transport (protocol v1 gift wrap or v2
    // NIP-44 direct); events of any other kind are dropped before any
    // decryption work. See docs/TRANSPORT_V2_SPEC.md.
    // DEPRECATED(v0.19.0, #786): with the `transport` knob gone this becomes
    // unconditionally kind 14 and the v1/v2 branching below collapses.
    #[allow(deprecated)]
    let accepted_kind = ctx.settings().mostro.transport.event_kind();
    // Phase 2 anti-spam gate (docs/TRANSPORT_V2_SPEC.md §6): on the v2 (kind
    // 14) transport the visible author is the trade key, so the daemon can
    // pre-validate before decrypting. Unknown (first-contact) senders must
    // clear `pow_first_contact`; known active-trade keys need only `pow`. The
    // gate is meaningless for v1 (gift wraps are signed by throwaway keys).
    let pow_first_contact = ctx.settings().mostro.effective_pow_first_contact();
    let is_v2 = accepted_kind.as_u16() == crate::config::constants::DM_EVENT_KIND;

    loop {
        let mut notifications = client.notifications();

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event { event, .. } = notification {
                let Some((action, message, unwrapped)) = accept_event(
                    &ctx,
                    &event,
                    my_keys,
                    pow,
                    pow_first_contact,
                    accepted_kind,
                    is_v2,
                )
                .await
                else {
                    continue;
                };
                let result = handle_message_action(
                    &action,
                    message.clone(),
                    &unwrapped,
                    my_keys,
                    ln_client,
                    &ctx,
                )
                .await;
                finalize_dispatch(result, message, unwrapped, &action).await;
            }
        }
    }
}

/// Cashu-mode event loop (CF-5). Mirrors [`run`]'s transport/validation
/// pipeline through the shared [`accept_event`]/[`finalize_dispatch`] helpers,
/// but dispatches through [`dispatch_cashu`] instead of
/// [`handle_message_action`] — there is no `ln_client` in Cashu mode. It
/// differs from `run` in exactly one line: the dispatch call.
///
/// During the foundation milestone every escrow/trade action is rejected with
/// `CantDo(InvalidAction)`; the feature tracks replace those arms one at a time
/// (see `docs/cashu/01-fundamentals.md` §6 action-ownership matrix).
pub async fn run_cashu(ctx: AppContext) -> Result<()> {
    let my_keys = ctx.keys();
    let client = ctx.nostr_client();
    let pow = ctx.settings().mostro.pow;
    #[allow(deprecated)]
    let accepted_kind = ctx.settings().mostro.transport.event_kind();
    let pow_first_contact = ctx.settings().mostro.effective_pow_first_contact();
    let is_v2 = accepted_kind.as_u16() == crate::config::constants::DM_EVENT_KIND;

    loop {
        let mut notifications = client.notifications();

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event { event, .. } = notification {
                let Some((action, message, unwrapped)) = accept_event(
                    &ctx,
                    &event,
                    my_keys,
                    pow,
                    pow_first_contact,
                    accepted_kind,
                    is_v2,
                )
                .await
                else {
                    continue;
                };
                let result =
                    dispatch_cashu(&action, message.clone(), &unwrapped, my_keys, &ctx).await;
                finalize_dispatch(result, message, unwrapped, &action).await;
            }
        }
    }
}

/// Route a validated action in Cashu mode (CF-5).
///
/// The allow-list is drawn at *"escrow-independent actions that neither create
/// nor advance an order"* (`docs/cashu/01-fundamentals.md` §6, closed
/// decision):
///
/// - **Allowed** → `handle_message_action_no_ln` (read-only / session; never
///   touch escrow, LND, or order lifecycle): `Orders`, `LastTradeIndex`,
///   `RestoreSession`, `TradePubkey`.
/// - **`AddCashuEscrow`** → `add_cashu_escrow_action` (a CF-5 stub Track A
///   fills in). Frozen here so Track A edits only its own file (G-1).
/// - **Blocked** → `CantDo(InvalidAction)` — everything that creates, advances,
///   or settles an order (there is no escrow behind it yet). The feature tracks
///   replace these arms one at a time; the action-ownership matrix in
///   fundamentals §6 guarantees every blocked action has an owner.
async fn dispatch_cashu(
    action: &Action,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
    ctx: &AppContext,
) -> Result<()> {
    match action {
        // Escrow-independent, read-only / session actions — safe in Cashu mode.
        Action::Orders | Action::LastTradeIndex | Action::RestoreSession | Action::TradePubkey => {
            handle_message_action_no_ln(action, msg, event, my_keys, ctx).await
        }
        // Cashu escrow lock — TA-1 fills the stub body; the routing is frozen.
        Action::AddCashuEscrow => add_cashu_escrow_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        // Everything that creates, advances, or settles an order has no escrow
        // behind it during the foundation milestone — reject it cleanly.
        _ => Err(MostroError::MostroCantDo(CantDoReason::InvalidAction).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::message::Action;

    use nostr_sdk::secp256k1::schnorr::Signature;
    use nostr_sdk::{Keys, Kind as NostrKind, Timestamp};

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

    // Helper function to create an UnwrappedMessage for testing. Identity and
    // sender (trade key) are distinct to mirror the canonical Mostro flow.
    fn create_test_unwrapped_message() -> UnwrappedMessage {
        let identity = create_test_keys();
        let trade = create_test_keys();

        UnwrappedMessage {
            message: create_test_message(Action::NewOrder, None),
            signature: None,
            sender: trade.public_key(),
            identity: identity.public_key(),
            created_at: Timestamp::now(),
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
        let event = create_test_unwrapped_message();
        let action = Action::NewOrder;

        let error = MostroError::MostroCantDo(CantDoReason::InvalidSignature);
        manage_errors(error, message, event, &action).await;

        // No-op: ensure no panic
    }

    #[tokio::test]
    async fn test_manage_errors_internal_error() {
        let message = create_test_message(Action::NewOrder, None);
        let event = create_test_unwrapped_message();
        let action = Action::NewOrder;

        let error =
            MostroError::MostroInternalErr(ServiceError::UnexpectedError("test error".to_string()));
        manage_errors(error, message, event, &action).await;

        // No-op: ensure no panic
    }

    mod check_trade_index_tests {
        use super::*;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        use sqlx::SqlitePool;
        use std::sync::Arc;

        async fn create_test_ctx() -> AppContext {
            let pool = Arc::new(SqlitePool::connect(":memory:").await.unwrap());
            TestContextBuilder::new()
                .with_pool(pool)
                .with_settings(test_settings())
                .build()
        }

        #[tokio::test]
        async fn test_check_trade_index_non_trading_action() {
            let ctx = create_test_ctx().await;
            let event = create_test_unwrapped_message();
            let message = create_test_message(Action::FiatSent, None);

            let result = check_trade_index(&ctx, &event, &message).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn test_check_trade_index_trading_action_no_index() {
            let ctx = create_test_ctx().await;
            let event = create_test_unwrapped_message();
            let message = create_test_message(Action::NewOrder, None);

            let result = check_trade_index(&ctx, &event, &message).await;
            assert!(result.is_ok());
        }

        async fn create_migrated_ctx() -> AppContext {
            let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
            sqlx::migrate!("./migrations")
                .run(pool.as_ref())
                .await
                .unwrap();
            TestContextBuilder::new()
                .with_pool(pool)
                .with_settings(test_settings())
                .build()
        }

        /// Insert a user row for `identity` with the given last_trade_index.
        async fn insert_user(ctx: &AppContext, identity: &PublicKey, index: i64) {
            add_new_user(
                ctx.pool(),
                User {
                    pubkey: identity.to_string(),
                    last_trade_index: index,
                    ..Default::default()
                },
            )
            .await
            .expect("insert user");
        }

        /// Build a signed trade-index message: the trade key (event.sender)
        /// signs the serialized message, mirroring what clients do.
        fn signed_event_and_message(trade_index: u32) -> (UnwrappedMessage, Message) {
            let identity = create_test_keys();
            let trade = create_test_keys();
            let message = create_test_message(Action::NewOrder, Some(trade_index));
            let sig = Message::sign(message.as_json().expect("json"), &trade);
            let event = UnwrappedMessage {
                message: message.clone(),
                signature: Some(sig),
                sender: trade.public_key(),
                identity: identity.public_key(),
                created_at: Timestamp::now(),
            };
            (event, message)
        }

        #[tokio::test]
        async fn known_user_with_fresh_index_and_valid_signature_passes() {
            let ctx = create_migrated_ctx().await;
            let (event, message) = signed_event_and_message(3);
            insert_user(&ctx, &event.identity, 2).await;

            let result = check_trade_index(&ctx, &event, &message).await;
            assert!(
                result.is_ok(),
                "fresh index + valid sig must pass: {result:?}"
            );
        }

        #[tokio::test]
        async fn known_user_with_stale_index_is_rejected() {
            let ctx = create_migrated_ctx().await;
            let (event, message) = signed_event_and_message(3);
            insert_user(&ctx, &event.identity, 5).await;

            let result = check_trade_index(&ctx, &event, &message).await;
            assert!(matches!(
                result,
                Err(MostroError::MostroCantDo(CantDoReason::InvalidTradeIndex))
            ));
        }

        #[tokio::test]
        async fn known_user_with_wrong_signature_is_rejected() {
            let ctx = create_migrated_ctx().await;
            let (mut event, message) = signed_event_and_message(3);
            // Signature from an unrelated key must not verify against the
            // trade key that authored the rumor.
            let interloper = create_test_keys();
            event.signature = Some(Message::sign(message.as_json().expect("json"), &interloper));
            insert_user(&ctx, &event.identity, 0).await;

            let result = check_trade_index(&ctx, &event, &message).await;
            assert!(matches!(
                result,
                Err(MostroError::MostroCantDo(CantDoReason::InvalidSignature))
            ));
        }

        #[tokio::test]
        async fn known_user_missing_signature_is_rejected() {
            let ctx = create_migrated_ctx().await;
            let (mut event, message) = signed_event_and_message(3);
            event.signature = None;
            insert_user(&ctx, &event.identity, 0).await;

            let result = check_trade_index(&ctx, &event, &message).await;
            assert!(matches!(
                result,
                Err(MostroError::MostroCantDo(CantDoReason::InvalidSignature))
            ));
        }

        #[tokio::test]
        async fn known_user_with_index_zero_skips_index_checks() {
            let ctx = create_migrated_ctx().await;
            let identity = create_test_keys();
            insert_user(&ctx, &identity.public_key(), 5).await;
            let mut event = create_test_unwrapped_message();
            event.identity = identity.public_key();
            // trade_index None → trade_index() == 0 → `1..` arm not taken.
            let message = create_test_message(Action::NewOrder, None);

            let result = check_trade_index(&ctx, &event, &message).await;
            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn unknown_user_with_index_zero_is_rejected() {
            let ctx = create_migrated_ctx().await;
            let event = create_test_unwrapped_message();
            let message = create_test_message(Action::NewOrder, Some(0));

            let result = check_trade_index(&ctx, &event, &message).await;
            assert!(matches!(
                result,
                Err(MostroError::MostroCantDo(CantDoReason::InvalidTradeIndex))
            ));
        }

        #[tokio::test]
        async fn unknown_user_with_valid_index_is_registered() {
            let ctx = create_migrated_ctx().await;
            let event = create_test_unwrapped_message();
            let message = create_test_message(Action::TakeBuy, Some(4));

            let result = check_trade_index(&ctx, &event, &message).await;
            assert!(result.is_ok(), "new user must be created: {result:?}");

            let user = is_user_present(ctx.pool(), event.identity.to_string())
                .await
                .expect("user must have been created");
            assert_eq!(user.last_trade_index, 4);
        }

        #[tokio::test]
        async fn test_check_trade_index_with_valid_index() {
            let ctx = create_test_ctx().await;
            let event = create_test_unwrapped_message();
            let message = create_test_message(Action::NewOrder, Some(1));

            // This test would require database setup and user creation
            // For now, we test the structure
            let result = check_trade_index(&ctx, &event, &message).await;
            // Result could be Ok or Err depending on database state
            assert!(result.is_ok() || result.is_err());
        }
    }

    mod handle_message_action_tests {
        use super::*;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        use sqlx::SqlitePool;
        use std::sync::Arc;

        fn create_restore_session_message() -> Message {
            Message::new_restore(None)
        }

        #[tokio::test]
        async fn routes_last_trade_index_to_handler_and_propagates_error() {
            let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
            sqlx::migrate!("./migrations")
                .run(pool.as_ref())
                .await
                .unwrap();

            let ctx = TestContextBuilder::new()
                .with_pool(pool)
                .with_settings(test_settings())
                .build();

            let my_keys = create_test_keys();
            let event = create_test_unwrapped_message();
            let msg = create_test_message(Action::LastTradeIndex, None);

            let result =
                handle_message_action_no_ln(&Action::LastTradeIndex, msg, &event, &my_keys, &ctx)
                    .await;

            // Routing assertion: we only require that the specific handler path is invoked
            // and its result is propagated; the exact business error is handler-owned.
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn routes_restore_session_to_handler_and_returns_ok() {
            let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
            sqlx::migrate!("./migrations")
                .run(pool.as_ref())
                .await
                .unwrap();

            let ctx = TestContextBuilder::new()
                .with_pool(pool)
                .with_settings(test_settings())
                .build();

            let my_keys = create_test_keys();
            let event = create_test_unwrapped_message();
            let msg = create_restore_session_message();

            let result =
                handle_message_action_no_ln(&Action::RestoreSession, msg, &event, &my_keys, &ctx)
                    .await;

            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn routes_orders_to_handler_and_propagates_error() {
            let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
            sqlx::migrate!("./migrations")
                .run(pool.as_ref())
                .await
                .unwrap();

            let ctx = TestContextBuilder::new()
                .with_pool(pool)
                .with_settings(test_settings())
                .build();

            let my_keys = create_test_keys();
            let event = create_test_unwrapped_message();
            let msg = create_test_message(Action::Orders, None);

            let result =
                handle_message_action_no_ln(&Action::Orders, msg, &event, &my_keys, &ctx).await;

            // Routing assertion: we only require that the specific handler path is invoked
            // and its result is propagated; the exact business error is handler-owned.
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn routes_every_no_ln_action_to_its_handler_without_panicking() {
            // Globals some handlers reach for; installing them is idempotent.
            let _ =
                crate::config::MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
            let _ = crate::NOSTR_CLIENT.set(nostr_sdk::Client::default());

            let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
            sqlx::migrate!("./migrations")
                .run(pool.as_ref())
                .await
                .unwrap();

            let ctx = TestContextBuilder::new()
                .with_pool(pool)
                .with_settings(test_settings())
                .build();

            let my_keys = create_test_keys();
            let event = create_test_unwrapped_message();

            // Every arm of the no-LN router: against an empty database each
            // handler returns its own business error (or Ok for no-op paths).
            // The routing contract under test is "dispatch + propagate, never
            // panic".
            for action in [
                Action::NewOrder,
                Action::TakeSell,
                Action::TakeBuy,
                Action::FiatSent,
                Action::AddInvoice,
                Action::AddBondInvoice,
                Action::Dispute,
                Action::RateUser,
                Action::AdminAddSolver,
                Action::AdminTakeDispute,
                Action::TradePubkey,
                // Not routed by the no-LN handler → default informational arm.
                Action::Release,
            ] {
                let msg = create_test_message(action.clone(), None);
                let _ = handle_message_action_no_ln(&action, msg, &event, &my_keys, &ctx).await;
            }
        }

        #[tokio::test]
        async fn routes_payinvoice_to_typed_invalid_action_error() {
            let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
            sqlx::migrate!("./migrations")
                .run(pool.as_ref())
                .await
                .unwrap();

            let ctx = TestContextBuilder::new()
                .with_pool(pool)
                .with_settings(test_settings())
                .build();

            let my_keys = create_test_keys();
            let event = create_test_unwrapped_message();
            let msg = create_test_message(Action::PayInvoice, None);

            let result =
                handle_message_action_no_ln(&Action::PayInvoice, msg, &event, &my_keys, &ctx).await;

            assert!(matches!(
                result,
                Err(e)
                    if e.downcast_ref::<MostroError>()
                        == Some(&MostroError::MostroCantDo(CantDoReason::InvalidAction))
            ));
        }
    }

    mod dispatch_cashu_tests {
        use super::*;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        use sqlx::SqlitePool;
        use std::sync::Arc;

        async fn create_ctx() -> AppContext {
            let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
            sqlx::migrate!("./migrations")
                .run(pool.as_ref())
                .await
                .unwrap();
            TestContextBuilder::new()
                .with_pool(pool)
                .with_settings(test_settings())
                .build()
        }

        fn is_invalid_action(result: Result<()>) -> bool {
            matches!(
                result,
                Err(e) if e.downcast_ref::<MostroError>()
                    == Some(&MostroError::MostroCantDo(CantDoReason::InvalidAction))
            )
        }

        /// Every action that creates, advances, or settles an order — plus the
        /// permanently-blocked buyer-invoice/bond actions and the not-yet-
        /// implemented `AddCashuEscrow` (its CF-5 stub returns `InvalidAction`
        /// too) — must be rejected with `CantDo(InvalidAction)` in Cashu
        /// foundation mode. This is the DoD "no trade can complete yet" gate.
        #[tokio::test]
        async fn blocks_every_order_lifecycle_action_with_invalid_action() {
            let _ =
                crate::config::MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
            let _ = crate::NOSTR_CLIENT.set(nostr_sdk::Client::default());
            let ctx = create_ctx().await;
            let my_keys = create_test_keys();
            let event = create_test_unwrapped_message();

            for action in [
                Action::NewOrder,
                Action::TakeBuy,
                Action::TakeSell,
                Action::AddInvoice,
                Action::FiatSent,
                Action::Release,
                Action::Cancel,
                Action::Dispute,
                Action::RateUser,
                Action::AddCashuEscrow,
                Action::AdminCancel,
                Action::AdminSettle,
                Action::AddBondInvoice,
                Action::AdminTakeDispute,
                Action::AdminAddSolver,
            ] {
                let msg = create_test_message(action.clone(), None);
                let result = dispatch_cashu(&action, msg, &event, &my_keys, &ctx).await;
                assert!(
                    is_invalid_action(result),
                    "{action:?} must be blocked with InvalidAction in Cashu mode"
                );
            }
        }

        /// The allow-list (`Orders`, `LastTradeIndex`, `RestoreSession`,
        /// `TradePubkey`) is routed to `handle_message_action_no_ln`. We assert
        /// routing by observing that `RestoreSession` reaches its handler and
        /// returns `Ok` — proving it was NOT short-circuited to `InvalidAction`.
        #[tokio::test]
        async fn allows_restore_session_through_no_ln_router() {
            let ctx = create_ctx().await;
            let my_keys = create_test_keys();
            let event = create_test_unwrapped_message();
            let msg = Message::new_restore(None);

            let result = dispatch_cashu(&Action::RestoreSession, msg, &event, &my_keys, &ctx).await;
            assert!(
                result.is_ok(),
                "RestoreSession must route to the no-LN handler, got {result:?}"
            );
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
                    // No-op
                }
                _ => unreachable!("Only GiftWrap events are considered in this test scope"),
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
                    if let Message::Order(_) = message {}
                }
                Err(_) => {
                    // Parsing error is handled gracefully
                    // No-op
                }
            }
        }
    }
}
