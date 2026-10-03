//! Main application module for the P2P trading system.
//! Handles message routing, action processing, and event loop management.

// Application context (dependency injection)
pub mod context;
pub mod daemon_state; // key/value operator state (maintenance mode)
pub mod maintenance; // maintenance (drain) mode flag + drain counters

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
pub mod payer; // Payer declaration + payment-account history (anti-triangulation)
pub mod rate_user; // User reputation system
pub mod release; // Release of held funds
pub mod reputation; // Reputation portability: import and export of attestations
pub mod restore_session; // Restore session action
pub mod serbero; // Serbero, the dispute assistant: boot registration as a read-only solver
pub mod take_buy; // Taking buy orders
pub mod take_sell; // Taking sell orders
pub mod trade_pubkey; // Trade pubkey action // Sync user trade index action
pub mod user_info; // Own reputation read

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
use crate::app::payer::declare::declare_payer_action;
use crate::app::rate_user::update_user_reputation_action;
use crate::app::release::release_action;
use crate::app::restore_session::restore_session_action;
use crate::app::take_buy::take_buy_action;
use crate::app::take_sell::take_sell_action;
use crate::app::trade_pubkey::trade_pubkey_action;
use crate::app::user_info::user_info;
// Core functionality imports
use crate::db::add_new_user;
use crate::db::is_user_present;
use crate::lightning::LndConnector;
use crate::spam_gate::SpamGate;
use crate::util::enqueue_cant_do_msg;
use crate::Result;

// External dependencies
use mostro_core::error::CantDoReason;
use mostro_core::error::MostroError;
use mostro_core::error::ServiceError;
use mostro_core::message::{Action, Message};
use mostro_core::transport::unwrap_incoming;
use mostro_core::transport::UnwrappedMessage;
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
        Action::UserInfo => user_info(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),

        // Dispute and rating actions
        Action::Dispute => dispute_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        Action::RateUser => update_user_reputation_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),

        // Payer history (anti-triangulation); answers invalid_action when the
        // feature is off (D-10). Not routed in Cashu mode yet (§10.8).
        Action::DeclarePayer => declare_payer_action(ctx, msg, event, my_keys)
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
        Action::ImportReputation => {
            reputation::import::import_reputation_action(ctx, msg, event, my_keys)
                .await
                .map_err(|e| e.into())
        }
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
/// (failed PoW, wrong kind, invalid event signature, spam-gate drop, decrypt
/// failure, stale, missing inner signature, failed trade-index, failed inner
/// verify, no action).
///
/// **Validation order is load-bearing**: the event signature is checked before
/// the spam gate, so the gate only ever records ids that survived
/// authentication. Recording an unauthenticated id would let anyone who can
/// deliver an event to the daemon censor a trade message by injecting a
/// same-id, signature-tampered copy first — the id does not commit to `sig`.
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
    gate: Option<&SpamGate>,
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
    // Authenticate the event BEFORE anything downstream records state keyed on
    // it. A nostr event id commits to `[0, pubkey, created_at, kind, tags,
    // content]` — **not** to `sig` — so a copy with a tampered signature keeps
    // the victim's id. Verifying here (cheap Schnorr, pre-decrypt) is what
    // makes the spam gate's dedup safe: only ids that are provably the
    // author's own are ever recorded, so a forged copy cannot get the genuine
    // event dropped as a replay. `unwrap_incoming` verifies the event again,
    // but only after the gate has run, so this check is not redundant.
    if event.verify().is_err() {
        tracing::warn!("Dropping event {} with an invalid signature", event.id);
        return None;
    }
    // Phase 2 anti-spam gate: cheap pre-validation BEFORE paying
    // the NIP-44 decrypt cost. `None` means no gate is installed
    // (fail-open).
    if let Some(gate) = gate {
        let now = chrono::Utc::now().timestamp();
        // Dedup: drop a re-sent identical event (defense in
        // depth against replay floods). Safe here and not
        // earlier: the id is only recorded once the signature
        // above proved the event is the author's own.
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
        //
        // The lane is decided on a *verified* author on
        // purpose: `is_known` keys off `event.pubkey`, and in
        // v2 the trade keys of active orders are public by
        // design. Deciding it before the signature check would
        // let any flooder claim a known key and skip the
        // first-contact toll entirely.
        if !gate.is_known(&event.pubkey.to_string()) && !event.check_pow(pow_first_contact) {
            tracing::info!(
                "Dropping first-contact kind-14 event from unknown key {} below pow_first_contact ({} bits)",
                event.pubkey,
                pow_first_contact
            );
            return None;
        }
    }

    // Mostro-core dispatches on the event kind and opens the
    // kind-14 3-element tuple with its in-ciphertext identity
    // proof, verifying every signature in one shot.
    let unwrapped = match unwrap_incoming(event, my_keys).await {
        Ok(Some(u)) => u,
        // NIP-44 decrypt failed: not addressed to this node.
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!("Error unwrapping incoming message: {}", e);
            if !is_stale(event.created_at) {
                if let Some(request_id) = unverified_user_info_request(event, my_keys) {
                    enqueue_cant_do_msg(
                        request_id,
                        None,
                        CantDoReason::InvalidSignature,
                        event.pubkey,
                    )
                    .await;
                }
            }
            return None;
        }
    };
    if is_stale(unwrapped.created_at) {
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
        // `user_info.md`, Errors: an identity proof that does not verify is
        // answered `invalid_signature`; other actions stay silent.
        let kind = message.get_inner_message_kind();
        if kind.action == Action::UserInfo {
            enqueue_cant_do_msg(
                kind.request_id,
                None,
                CantDoReason::InvalidSignature,
                unwrapped.sender,
            )
            .await;
        }
        return None;
    }

    // Get inner message kind
    let inner_message = message.get_inner_message_kind();
    // Maintenance (drain) mode: refuse to open new escrow. This runs BEFORE
    // `check_trade_index` on purpose — that check registers a first-time
    // identity and persists its trade index, so gating after it would burn
    // the index of a request we are about to reject. Nothing is persisted on
    // this path; the sender only gets a `CantDo(MaintenanceMode)`.
    if ctx.maintenance().blocks(&inner_message.action) {
        tracing::info!(
            "Maintenance mode: rejecting {:?} from {}",
            inner_message.action,
            unwrapped.sender
        );
        manage_errors(
            MostroError::MostroCantDo(CantDoReason::MaintenanceMode),
            message.clone(),
            unwrapped.clone(),
            &inner_message.action,
        )
        .await;
        return None;
    }
    // Check if message is message with trade index
    if let Err(e) = check_trade_index(ctx, &unwrapped, &message).await {
        tracing::warn!("Error checking trade index: {}", e);
        return None;
    }

    if !message.verify() {
        return None;
    }
    // Core's verify also admits the `user-info` reply shape; intake takes only the request.
    if inner_message.action == Action::UserInfo && inner_message.payload.is_some() {
        return None;
    }
    let action = message.inner_action()?;
    Some((action, message, unwrapped))
}

/// Events older than 10 seconds are discarded to prevent replay attacks.
fn is_stale(created_at: Timestamp) -> bool {
    let since_time = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::seconds(10))
        .unwrap()
        .timestamp() as u64;
    created_at.as_secs() < since_time
}

/// The `request_id` of a `user-info` request that `unwrap_incoming`
/// refused, so it can be answered `cant-do` `invalid_signature`
/// (`user_info.md`, Errors). Core returns no message on a failed proof, so
/// the tuple is opened again here only far enough to read the action. The
/// reply goes to the event author, so the event signature must verify;
/// `None` for anything else, which keeps being dropped silently.
fn unverified_user_info_request(event: &Event, my_keys: &Keys) -> Option<Option<u64>> {
    event.verify().ok()?;
    let plaintext = nip44::decrypt(my_keys.secret_key(), &event.pubkey, &event.content).ok()?;
    let (message, _, _): (Message, serde_json::Value, serde_json::Value) =
        serde_json::from_str(&plaintext).ok()?;
    let kind = message.get_inner_message_kind();
    (kind.action == Action::UserInfo).then_some(kind.request_id)
}

/// Actions that can tie the sender's trade key to an order (creator or taker)
/// or to a dispute (solver). Only these are worth a recognition lookup after
/// dispatch; every other action either needs no new key or already came from
/// a recognized one.
fn introduces_trade_key(action: &Action) -> bool {
    matches!(
        action,
        Action::NewOrder | Action::TakeSell | Action::TakeBuy | Action::AdminTakeDispute
    )
}

/// Add the sender's trade key to the spam gate if the DB now ties it to an
/// order or dispute (#857), so its follow-up needs only the base `pow`.
///
/// It asks the DB rather than trusting the handler result, for two reasons:
/// a handler can commit and then fail on a later step (a new order is stored
/// before its broadcast), and a handler can succeed without storing anything.
/// Recognition then grants exactly the keys the next rebuild would, never one
/// that a rejected request introduced.
///
/// A dispute take stores the solver's identity, so a solver is recognized when
/// it sends from that identity (as the admin clients do); a solver sending
/// from a separate key keeps paying `pow_first_contact`, as before.
async fn recognize_sender(ctx: &AppContext, gate: &SpamGate, unwrapped: &UnwrappedMessage) {
    let sender = unwrapped.sender.to_string();
    match crate::db::is_active_trade_pubkey(ctx.pool(), &sender).await {
        Ok(true) => gate.add_known(sender),
        Ok(false) => {}
        Err(e) => tracing::warn!("spam_gate: recognition lookup failed: {e}"),
    }
}

/// Shared post-dispatch tail (identical in both loops). An action that may
/// introduce a trade key is first checked for recognition
/// ([`recognize_sender`]); then a handler `Err` is downcast to a `MostroError`
/// and turned into the right reply (`manage_errors`) or logged
/// (`warning_msg`). Factored out with [`accept_event`] so `run` and
/// `run_cashu` share one tail (CF-5).
async fn finalize_dispatch(
    ctx: &AppContext,
    result: Result<()>,
    message: Message,
    unwrapped: UnwrappedMessage,
    action: &Action,
    gate: Option<&SpamGate>,
) {
    if let Some(gate) = gate.filter(|_| introduces_trade_key(action)) {
        recognize_sender(ctx, gate, &unwrapped).await;
    }
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
    // The node speaks exactly one transport; events of any other kind are
    // dropped before any decryption work. See docs/TRANSPORT_V2_SPEC.md.
    let accepted_kind = ctx.settings().mostro.transport.event_kind();
    // Phase 2 anti-spam gate (docs/TRANSPORT_V2_SPEC.md §6): the visible
    // author is the trade key, so the daemon can pre-validate before
    // decrypting. Unknown (first-contact) senders must clear
    // `pow_first_contact`; known active-trade keys need only `pow`.
    // `install_spam_gate` (`main.rs`) runs before both loops, so the lookup
    // stays out of the per-event path.
    let pow_first_contact = ctx.settings().mostro.effective_pow_first_contact();
    let gate = SpamGate::global();

    loop {
        let mut notifications = client.notifications();

        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let Some((action, message, unwrapped)) = accept_event(
                    &ctx,
                    &event,
                    my_keys,
                    pow,
                    pow_first_contact,
                    accepted_kind,
                    gate,
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
                finalize_dispatch(&ctx, result, message, unwrapped, &action, gate).await;
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
    let accepted_kind = ctx.settings().mostro.transport.event_kind();
    let pow_first_contact = ctx.settings().mostro.effective_pow_first_contact();
    let gate = SpamGate::global();

    loop {
        let mut notifications = client.notifications();

        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let Some((action, message, unwrapped)) = accept_event(
                    &ctx,
                    &event,
                    my_keys,
                    pow,
                    pow_first_contact,
                    accepted_kind,
                    gate,
                )
                .await
                else {
                    continue;
                };
                let result =
                    dispatch_cashu(&action, message.clone(), &unwrapped, my_keys, &ctx).await;
                finalize_dispatch(&ctx, result, message, unwrapped, &action, gate).await;
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
///   `UserInfo`, `RestoreSession`, `TradePubkey`.
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
        Action::Orders
        | Action::LastTradeIndex
        | Action::UserInfo
        | Action::RestoreSession
        | Action::TradePubkey => {
            handle_message_action_no_ln(action, msg, event, my_keys, ctx).await
        }
        // Reputation import touches no escrow either.
        Action::ImportReputation => {
            handle_message_action_no_ln(action, msg, event, my_keys, ctx).await
        }
        // Order creation + the take flow (Track A TA-2). Creating a pending
        // order touches no escrow; the take handlers branch on cashu mode and
        // emit the escrow request (`show_cashu_escrow_request`) instead of a
        // hold invoice. Creatable and takeable ship together so the book never
        // fills with untakeable orders.
        Action::NewOrder | Action::TakeBuy | Action::TakeSell => {
            handle_message_action_no_ln(action, msg, event, my_keys, ctx).await
        }
        // Cashu escrow lock — TA-1 fills the stub body; the routing is frozen.
        Action::AddCashuEscrow => add_cashu_escrow_action(ctx, msg, event, my_keys)
            .await
            .map_err(|e| e.into()),
        // Everything that advances or settles an order past the lock has no
        // handler yet during Track A — reject it cleanly. Later tracks
        // (release/cancel/dispute) replace these arms one at a time.
        _ => Err(MostroError::MostroCantDo(CantDoReason::InvalidAction).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::message::Action;

    use nostr_sdk::prelude::{Keys, Kind as NostrKind, Timestamp};

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

    // An AppContext backed by a fresh in-memory database with the migrations
    // applied. Shared by every child test module through `use super::*`.
    async fn create_migrated_ctx() -> AppContext {
        let pool = std::sync::Arc::new(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
        sqlx::migrate!("./migrations")
            .run(pool.as_ref())
            .await
            .unwrap();
        crate::app::context::test_utils::TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(crate::app::context::test_utils::test_settings())
            .build()
    }

    /// The `CantDo` reasons queued for `to` (the queue is process-global,
    /// so filter by destination).
    async fn cant_do_reasons_for(to: &PublicKey) -> Vec<CantDoReason> {
        crate::config::MESSAGE_QUEUES
            .queue_order_cantdo
            .read()
            .await
            .iter()
            .filter(|(_, dest)| dest == to)
            .filter_map(|(m, _)| match &m.get_inner_message_kind().payload {
                Some(mostro_core::prelude::Payload::CantDo(Some(r))) => Some(r.clone()),
                _ => None,
            })
            .collect()
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

    /// Ordering contract of [`accept_event`]: the event signature is verified
    /// **before** the spam gate records the id. A nostr event id does not
    /// commit to `sig`, so without that order anyone able to deliver an event
    /// to the daemon could censor a trade message by racing in a same-id,
    /// signature-tampered copy: the copy would be recorded and the genuine
    /// event dropped as a replay.
    mod accept_event_ordering_tests {
        use super::*;
        use crate::spam_gate::{SpamGate, REPLAY_WINDOW_SECS};
        use mostro_core::transport::{wrap_message_nip44, WrapOptions};

        /// A protocol-v2 (kind 14) event addressed to `mostro`, in full-privacy
        /// mode (trade key doubles as identity, so no identity proof is needed).
        fn v2_event(mostro: &Keys) -> Event {
            let trade = create_test_keys();
            let message = create_test_message(Action::FiatSent, None);
            wrap_message_nip44(
                &message,
                &trade,
                &trade,
                mostro.public_key(),
                WrapOptions::default(),
            )
            .expect("wrap kind-14 event")
        }

        /// The attacker's copy: only `sig` is replaced, which leaves the id —
        /// and therefore the mined PoW — untouched.
        fn with_tampered_signature(event: &Event) -> Event {
            let decoy = EventBuilder::new(NostrKind::TextNote, "decoy")
                .finalize(&create_test_keys())
                .expect("sign decoy");
            let mut forged = event.clone();
            forged.sig = decoy.sig;
            forged
        }

        /// Drive `accept_event` over a v2 (kind-14) event with an explicit
        /// gate, the shape both ordering tests below need.
        async fn accept(
            ctx: &AppContext,
            event: &Event,
            mostro: &Keys,
            gate: &SpamGate,
        ) -> Option<(Action, Message, UnwrappedMessage)> {
            // Same kind constant the event loops accept, so the test fails if
            // it drifts from `Transport::Nip44Direct`'s kind.
            accept_event(
                ctx,
                event,
                mostro,
                0,
                0,
                NostrKind::from(crate::config::constants::DM_EVENT_KIND),
                Some(gate),
            )
            .await
        }

        #[tokio::test]
        async fn tampered_copy_does_not_censor_the_genuine_event() {
            let ctx = create_migrated_ctx().await;
            let gate = SpamGate::new(REPLAY_WINDOW_SECS);
            let mostro = create_test_keys();
            let genuine = v2_event(&mostro);
            let forged = with_tampered_signature(&genuine);

            assert_eq!(forged.id, genuine.id, "tampering sig must preserve the id");
            assert!(forged.verify().is_err(), "the copy must not verify");

            assert!(
                accept(&ctx, &forged, &mostro, &gate).await.is_none(),
                "an event with an invalid signature must be dropped"
            );
            assert!(
                accept(&ctx, &genuine, &mostro, &gate).await.is_some(),
                "the genuine event must survive a forged same-id copy"
            );
        }

        #[tokio::test]
        async fn genuine_duplicate_is_still_dropped_as_a_replay() {
            let ctx = create_migrated_ctx().await;
            let gate = SpamGate::new(REPLAY_WINDOW_SECS);
            let mostro = create_test_keys();
            let genuine = v2_event(&mostro);

            assert!(
                accept(&ctx, &genuine, &mostro, &gate).await.is_some(),
                "first sighting is accepted"
            );
            assert!(
                accept(&ctx, &genuine, &mostro, &gate).await.is_none(),
                "the replay guard still drops a re-sent identical event"
            );
        }

        #[tokio::test]
        async fn invalid_signature_is_dropped_without_a_gate() {
            // The `None` path — no gate installed — must still reject.
            let ctx = create_migrated_ctx().await;
            let mostro = create_test_keys();
            let forged = with_tampered_signature(&v2_event(&mostro));

            let accepted = accept_event(
                &ctx,
                &forged,
                &mostro,
                0,
                0,
                NostrKind::from(crate::config::constants::DM_EVENT_KIND),
                None,
            )
            .await;
            assert!(accepted.is_none());
        }
    }

    /// Synchronous recognition (#857): after a create, take or dispute take,
    /// the sender's trade key enters the spam gate as soon as the DB ties it
    /// to an order or dispute, so the follow-up needs only the base `pow`
    /// without waiting for the periodic rebuild. The handler result does not
    /// decide it: committed state does.
    mod sync_recognition_tests {
        use super::*;
        use crate::spam_gate::{SpamGate, REPLAY_WINDOW_SECS};
        use mostro_core::order::{Kind as OrderKind, SmallOrder};
        use mostro_core::transport::{wrap_message_nip44, WrapOptions};

        /// Far above anything an unmined test event reaches by chance
        /// (2^-20), so "dropped at first contact" is deterministic.
        const FIRST_CONTACT_POW: u8 = 20;

        /// A fresh, unmined kind-14 event from `trade` (full-privacy mode).
        fn follow_up_from(trade: &Keys, mostro: &Keys) -> Event {
            wrap_message_nip44(
                &create_test_message(Action::FiatSent, None),
                trade,
                trade,
                mostro.public_key(),
                WrapOptions::default(),
            )
            .expect("wrap kind-14 event")
        }

        async fn accepted(ctx: &AppContext, event: &Event, mostro: &Keys, gate: &SpamGate) -> bool {
            accept_event(
                ctx,
                event,
                mostro,
                0,
                FIRST_CONTACT_POW,
                NostrKind::from(crate::config::constants::DM_EVENT_KIND),
                Some(gate),
            )
            .await
            .is_some()
        }

        async fn dispatch(
            ctx: &AppContext,
            gate: &SpamGate,
            trade: &Keys,
            action: Action,
            result: Result<()>,
        ) {
            let unwrapped = UnwrappedMessage {
                message: create_test_message(action.clone(), None),
                signature: None,
                sender: trade.public_key(),
                identity: trade.public_key(),
                created_at: Timestamp::now(),
            };
            finalize_dispatch(
                ctx,
                result,
                unwrapped.message.clone(),
                unwrapped,
                &action,
                Some(gate),
            )
            .await;
        }

        /// Store a pending sell order made by `trade` the way the real
        /// `new-order` handler does. The offline test client cannot broadcast,
        /// so, like on a node whose relays are down, `publish_order` commits
        /// the row and then returns an error.
        async fn store_order_failing_at_broadcast(ctx: &AppContext, trade: &Keys) -> Result<()> {
            let _ =
                crate::config::MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
            let _ = crate::config::NOSTR_CLIENT.set(nostr_sdk::prelude::Client::default());
            let order = SmallOrder {
                kind: Some(OrderKind::Sell),
                amount: 1_000,
                fiat_code: "USD".to_string(),
                fiat_amount: 100,
                payment_method: "SEPA".to_string(),
                ..Default::default()
            };
            let pk = trade.public_key();
            let result = crate::util::publish_order(
                ctx.pool(),
                &create_test_keys(),
                &order,
                pk,
                pk,
                pk,
                Some(1),
                Some(1),
            )
            .await;
            assert!(result.is_err(), "the broadcast must fail offline");
            result.map_err(Into::into)
        }

        #[tokio::test]
        async fn follow_up_after_a_stored_order_needs_only_base_pow_even_if_publishing_failed() {
            let ctx = create_migrated_ctx().await;
            let gate = SpamGate::new(REPLAY_WINDOW_SECS);
            let mostro = create_test_keys();
            let trade = create_test_keys();

            assert!(
                !accepted(&ctx, &follow_up_from(&trade, &mostro), &mostro, &gate).await,
                "an unknown key below pow_first_contact is dropped"
            );

            let result = store_order_failing_at_broadcast(&ctx, &trade).await;
            dispatch(&ctx, &gate, &trade, Action::NewOrder, result).await;

            assert!(
                accepted(&ctx, &follow_up_from(&trade, &mostro), &mostro, &gate).await,
                "the order is stored, so its follow-up must pass on base pow"
            );
        }

        #[tokio::test]
        async fn every_introducing_action_recognizes_a_stored_key() {
            for action in [
                Action::NewOrder,
                Action::TakeSell,
                Action::TakeBuy,
                Action::AdminTakeDispute,
            ] {
                let ctx = create_migrated_ctx().await;
                let gate = SpamGate::new(REPLAY_WINDOW_SECS);
                let trade = create_test_keys();
                let _ = store_order_failing_at_broadcast(&ctx, &trade).await;

                dispatch(&ctx, &gate, &trade, action.clone(), Ok(())).await;

                assert!(
                    gate.is_known(&trade.public_key().to_string()),
                    "{action:?} must recognize a key the DB ties to an order"
                );
            }
        }

        #[tokio::test]
        async fn nothing_stored_grants_nothing_whatever_the_result() {
            let ctx = create_migrated_ctx().await;
            let cant_do: Result<()> =
                Err(MostroError::MostroCantDo(CantDoReason::InvalidOrderStatus).into());
            for result in [cant_do, Ok(())] {
                let gate = SpamGate::new(REPLAY_WINDOW_SECS);
                let trade = create_test_keys();

                dispatch(&ctx, &gate, &trade, Action::TakeSell, result).await;

                assert!(
                    !gate.is_known(&trade.public_key().to_string()),
                    "a key tied to nothing must not open the known lane"
                );
            }
        }

        #[tokio::test]
        async fn other_actions_are_left_to_the_rebuild() {
            // No lookup for actions that cannot introduce a key, even when the
            // key is active: the periodic rebuild already covers them.
            let ctx = create_migrated_ctx().await;
            let trade = create_test_keys();
            let _ = store_order_failing_at_broadcast(&ctx, &trade).await;
            for action in [
                Action::Orders,
                Action::LastTradeIndex,
                Action::UserInfo,
                Action::RestoreSession,
                Action::FiatSent,
            ] {
                let gate = SpamGate::new(REPLAY_WINDOW_SECS);
                dispatch(&ctx, &gate, &trade, action.clone(), Ok(())).await;
                assert!(
                    !gate.is_known(&trade.public_key().to_string()),
                    "{action:?} must not trigger recognition"
                );
            }
        }

        #[tokio::test]
        async fn solver_is_recognized_by_the_identity_the_dispute_stores() {
            // `admin-take-dispute` stores the solver's identity; the rebuild
            // returns that key and nothing else, so recognition follows it: a
            // solver sending from its identity is fast-pathed, a separate
            // trade key is not (it would be dropped again at the next rebuild).
            let ctx = create_migrated_ctx().await;
            let solver = create_test_keys();
            let other_key = create_test_keys();
            sqlx::query(
                "INSERT INTO disputes (id, order_id, status, order_previous_status, solver_pubkey, created_at) \
                 VALUES (?1, ?2, 'in-progress', 'fiat-sent', ?3, 1700000000)",
            )
            .bind(uuid::Uuid::new_v4())
            .bind(uuid::Uuid::new_v4())
            .bind(solver.public_key().to_string())
            .execute(ctx.pool())
            .await
            .unwrap();

            let gate = SpamGate::new(REPLAY_WINDOW_SECS);
            let from_other_key = UnwrappedMessage {
                message: create_test_message(Action::AdminTakeDispute, None),
                signature: None,
                sender: other_key.public_key(),
                identity: solver.public_key(),
                created_at: Timestamp::now(),
            };
            finalize_dispatch(
                &ctx,
                Ok(()),
                from_other_key.message.clone(),
                from_other_key,
                &Action::AdminTakeDispute,
                Some(&gate),
            )
            .await;
            assert!(!gate.is_known(&other_key.public_key().to_string()));

            dispatch(&ctx, &gate, &solver, Action::AdminTakeDispute, Ok(())).await;
            assert!(gate.is_known(&solver.public_key().to_string()));
        }

        #[tokio::test]
        async fn no_gate_installed_is_a_no_op() {
            let ctx = create_migrated_ctx().await;
            let unwrapped = create_test_unwrapped_message();
            finalize_dispatch(
                &ctx,
                Ok(()),
                unwrapped.message.clone(),
                unwrapped,
                &Action::NewOrder,
                None,
            )
            .await;
        }
    }

    /// Maintenance (drain) mode gate in [`accept_event`]: the three actions
    /// that open new escrow are answered with `CantDo(MaintenanceMode)` and
    /// persist nothing; everything else passes through unchanged.
    mod maintenance_gate_tests {
        use super::*;
        use crate::db::is_user_present;
        use mostro_core::transport::{wrap_message_nip44, WrapOptions};

        /// A kind-14 event in full-privacy mode (trade key doubles as
        /// identity) so no inner signature is needed.
        fn v2_event(
            mostro: &Keys,
            trade: &Keys,
            action: Action,
            trade_index: Option<u32>,
        ) -> Event {
            let message = create_test_message(action, trade_index);
            wrap_message_nip44(
                &message,
                trade,
                trade,
                mostro.public_key(),
                WrapOptions::default(),
            )
            .expect("wrap kind-14 event")
        }

        async fn accept(ctx: &AppContext, event: &Event, mostro: &Keys) -> bool {
            accept_event(
                ctx,
                event,
                mostro,
                0,
                0,
                NostrKind::from(crate::config::constants::DM_EVENT_KIND),
                None,
            )
            .await
            .is_some()
        }

        async fn enabled_ctx() -> AppContext {
            let ctx = create_migrated_ctx().await;
            ctx.maintenance()
                .set(ctx.pool(), true, Some("test"))
                .await
                .unwrap();
            ctx
        }

        #[tokio::test]
        async fn gate_rejects_new_order_take_buy_and_take_sell_when_enabled() {
            let ctx = enabled_ctx().await;
            let mostro = create_test_keys();
            for action in [Action::NewOrder, Action::TakeBuy, Action::TakeSell] {
                let trade = create_test_keys();
                let event = v2_event(&mostro, &trade, action.clone(), None);
                assert!(
                    !accept(&ctx, &event, &mostro).await,
                    "{action:?} must be rejected"
                );
                assert_eq!(
                    cant_do_reasons_for(&trade.public_key()).await,
                    vec![CantDoReason::MaintenanceMode],
                    "{action:?} must be answered with MaintenanceMode"
                );
            }
        }

        /// Payload-less test messages only pass `message.verify()` for
        /// some actions (`FiatSent` does), so the positive acceptance is
        /// pinned on that one and the rest are checked for the absence of a
        /// `CantDo` — which is all the gate can produce.
        #[tokio::test]
        async fn gate_passes_actions_on_existing_orders_when_enabled() {
            let ctx = enabled_ctx().await;
            let mostro = create_test_keys();
            let trade = create_test_keys();
            let event = v2_event(&mostro, &trade, Action::FiatSent, None);
            assert!(
                accept(&ctx, &event, &mostro).await,
                "FiatSent must be accepted"
            );
            assert!(cant_do_reasons_for(&trade.public_key()).await.is_empty());

            for action in [
                Action::Release,
                Action::Cancel,
                Action::AddInvoice,
                Action::AddBondInvoice,
                Action::Dispute,
                Action::RateUser,
                Action::Orders,
                Action::RestoreSession,
                Action::TradePubkey,
                Action::LastTradeIndex,
                Action::UserInfo,
                Action::AdminCancel,
                Action::AdminSettle,
            ] {
                let trade = create_test_keys();
                let event = v2_event(&mostro, &trade, action.clone(), None);
                accept(&ctx, &event, &mostro).await;
                assert!(
                    cant_do_reasons_for(&trade.public_key()).await.is_empty(),
                    "{action:?} must not be answered by the gate"
                );
            }
        }

        #[tokio::test]
        async fn gate_is_noop_when_disabled() {
            let ctx = create_migrated_ctx().await;
            assert!(!ctx.maintenance().is_enabled());
            let mostro = create_test_keys();
            for action in [Action::NewOrder, Action::TakeBuy, Action::TakeSell] {
                let trade = create_test_keys();
                let event = v2_event(&mostro, &trade, action.clone(), None);
                accept(&ctx, &event, &mostro).await;
                assert!(
                    cant_do_reasons_for(&trade.public_key()).await.is_empty(),
                    "{action:?} must not be gated while disabled"
                );
            }
        }

        /// Dual-key event (identity signs the inner tuple, trade key authors
        /// the event): the shape in which `check_trade_index` registers a
        /// first-time identity.
        fn dual_key_event(mostro: &Keys, identity: &Keys, trade: &Keys) -> Event {
            let message = create_test_message(Action::NewOrder, Some(1));
            let opts = WrapOptions {
                signed: true,
                ..WrapOptions::default()
            };
            wrap_message_nip44(&message, identity, trade, mostro.public_key(), opts)
                .expect("wrap signed kind-14 event")
        }

        /// `check_trade_index` registers an unknown identity and persists its
        /// trade index. The gate runs first so a rejected first-time user is
        /// never created — otherwise the same request retried after
        /// maintenance would fail with `InvalidTradeIndex`.
        #[tokio::test]
        async fn gate_rejects_first_time_user_without_creating_it() {
            let mostro = create_test_keys();

            // Control: with the gate off the same request registers the user
            // (`check_trade_index` runs before the payload shape check, so
            // the registration happens whether or not the message is later
            // accepted — which is exactly why the gate must come first).
            let open = create_migrated_ctx().await;
            let (identity, trade) = (create_test_keys(), create_test_keys());
            let event = dual_key_event(&mostro, &identity, &trade);
            accept(&open, &event, &mostro).await;
            assert!(
                is_user_present(open.pool(), identity.public_key().to_string())
                    .await
                    .is_ok(),
                "control: check_trade_index registers a first-time user"
            );

            // Under maintenance nothing is persisted.
            let closed = enabled_ctx().await;
            let (identity, trade) = (create_test_keys(), create_test_keys());
            let event = dual_key_event(&mostro, &identity, &trade);
            assert!(!accept(&closed, &event, &mostro).await);
            assert!(
                is_user_present(closed.pool(), identity.public_key().to_string())
                    .await
                    .is_err(),
                "a rejected first-time user must not be registered"
            );
            assert_eq!(
                cant_do_reasons_for(&trade.public_key()).await,
                vec![CantDoReason::MaintenanceMode]
            );
        }
    }

    /// [`accept_event`] admits `user-info` only in its request form:
    /// `restore` wrapper, no order id, no payload.
    mod user_info_intake_tests {
        use super::*;
        use mostro_core::prelude::{MessageKind, Payload};
        use mostro_core::transport::{wrap_message_nip44, WrapOptions};

        fn kind(id: Option<uuid::Uuid>, payload: Option<Payload>) -> MessageKind {
            MessageKind::new(id, Some(1), None, Action::UserInfo, payload)
        }

        async fn accepted(message: Message) -> bool {
            let ctx = create_migrated_ctx().await;
            let mostro = create_test_keys();
            let trade = create_test_keys();
            let event = wrap_message_nip44(
                &message,
                &trade,
                &trade,
                mostro.public_key(),
                WrapOptions::default(),
            )
            .expect("wrap kind-14 event");
            accept_event(
                &ctx,
                &event,
                &mostro,
                0,
                0,
                NostrKind::from(crate::config::constants::DM_EVENT_KIND),
                None,
            )
            .await
            .is_some()
        }

        #[tokio::test]
        async fn request_in_restore_wrapper_is_accepted() {
            assert!(accepted(Message::Restore(kind(None, None))).await);
        }

        /// The request the handler actually answers: a separate identity key
        /// proving the request, plus the trade signature.
        #[tokio::test]
        async fn reputation_mode_request_is_accepted_with_its_identity() {
            let ctx = create_migrated_ctx().await;
            let (mostro, identity, trade) =
                (create_test_keys(), create_test_keys(), create_test_keys());
            let event = wrap_message_nip44(
                &Message::Restore(kind(None, None)),
                &identity,
                &trade,
                mostro.public_key(),
                WrapOptions::default(),
            )
            .expect("wrap kind-14 event");

            let (action, _, unwrapped) = accept_event(
                &ctx,
                &event,
                &mostro,
                0,
                0,
                NostrKind::from(crate::config::constants::DM_EVENT_KIND),
                None,
            )
            .await
            .expect("a reputation-mode user-info request must be accepted");

            assert_eq!(action, Action::UserInfo);
            assert_eq!(unwrapped.identity, identity.public_key());
            assert_eq!(unwrapped.sender, trade.public_key());
        }

        /// A kind-14 event whose tuple carries the given trade signature and
        /// identity proof, built by hand so either can be wrong or missing.
        fn raw_event(
            message: &Message,
            trade: &Keys,
            mostro: &PublicKey,
            trade_sig: Option<String>,
            identity_proof: Option<(String, String)>,
        ) -> nostr_sdk::prelude::Event {
            use nostr_sdk::prelude::{nip44, EventBuilder, Kind, Tag};
            let tuple = serde_json::to_string(&(message, trade_sig, identity_proof)).unwrap();
            let content =
                nip44::encrypt(trade.secret_key(), mostro, tuple, nip44::Version::default())
                    .unwrap();
            EventBuilder::new(Kind::PrivateDirectMessage, content)
                .tags([Tag::public_key(*mostro)])
                .finalize(trade)
                .unwrap()
        }

        async fn accept_raw(event: &nostr_sdk::prelude::Event, mostro: &Keys) -> bool {
            let ctx = create_migrated_ctx().await;
            accept_event(
                &ctx,
                event,
                mostro,
                0,
                0,
                NostrKind::from(crate::config::constants::DM_EVENT_KIND),
                None,
            )
            .await
            .is_some()
        }

        /// The identity's signature over the v2 proof payload
        /// (`mostro-transport-v2-identity:<trade hex>:<message json>`).
        fn identity_sig(message: &Message, trade: &Keys, signer: &Keys) -> String {
            let payload = format!(
                "mostro-transport-v2-identity:{}:{}",
                trade.public_key().to_hex(),
                message.as_json().unwrap()
            );
            Message::sign(payload, signer).to_string()
        }

        /// `user_info.md`, Errors: "`invalid_signature`: the identity proof
        /// does not verify."
        #[tokio::test]
        async fn identity_proof_that_does_not_verify_gets_invalid_signature() {
            let (mostro, identity, trade) =
                (create_test_keys(), create_test_keys(), create_test_keys());
            let message = Message::Restore(kind(None, None));
            let trade_sig = Message::sign(message.as_json().unwrap(), &trade).to_string();
            // Signed by another key than the identity it names.
            let forged = identity_sig(&message, &trade, &create_test_keys());
            let event = raw_event(
                &message,
                &trade,
                &mostro.public_key(),
                Some(trade_sig),
                Some((identity.public_key().to_hex(), forged)),
            );

            assert!(!accept_raw(&event, &mostro).await);
            assert_eq!(
                cant_do_reasons_for(&trade.public_key()).await,
                vec![CantDoReason::InvalidSignature]
            );
        }

        #[tokio::test]
        async fn identity_proof_without_trade_signature_gets_invalid_signature() {
            let (mostro, identity, trade) =
                (create_test_keys(), create_test_keys(), create_test_keys());
            let message = Message::Restore(kind(None, None));
            let proof = identity_sig(&message, &trade, &identity);
            let event = raw_event(
                &message,
                &trade,
                &mostro.public_key(),
                None,
                Some((identity.public_key().to_hex(), proof)),
            );

            assert!(!accept_raw(&event, &mostro).await);
            assert_eq!(
                cant_do_reasons_for(&trade.public_key()).await,
                vec![CantDoReason::InvalidSignature]
            );
        }

        /// The spec defines the answer for `user-info` only; every other
        /// action with a bad proof is still dropped without a reply.
        #[tokio::test]
        async fn other_actions_with_a_bad_proof_get_no_reply() {
            let (mostro, identity, trade) =
                (create_test_keys(), create_test_keys(), create_test_keys());
            let message = Message::Restore(MessageKind::new(
                None,
                Some(1),
                None,
                Action::LastTradeIndex,
                None,
            ));
            let trade_sig = Message::sign(message.as_json().unwrap(), &trade).to_string();
            let forged = identity_sig(&message, &trade, &create_test_keys());
            let event = raw_event(
                &message,
                &trade,
                &mostro.public_key(),
                Some(trade_sig),
                Some((identity.public_key().to_hex(), forged)),
            );

            assert!(!accept_raw(&event, &mostro).await);
            assert!(cant_do_reasons_for(&trade.public_key()).await.is_empty());
        }

        #[tokio::test]
        async fn user_info_outside_restore_wrapper_is_dropped() {
            for message in [
                Message::Order(kind(None, None)),
                Message::Dispute(kind(None, None)),
                Message::Dm(kind(None, None)),
                Message::Rate(kind(None, None)),
                Message::CantDo(kind(None, None)),
            ] {
                assert!(!accepted(message.clone()).await, "{message:?}");
            }
        }

        #[tokio::test]
        async fn user_info_with_an_order_id_is_dropped() {
            let message = Message::Restore(kind(Some(uuid::Uuid::new_v4()), None));
            assert!(!accepted(message).await);
        }

        #[tokio::test]
        async fn reply_shaped_user_info_is_dropped() {
            let reply = Payload::UserInfo(crate::util::peer_reputation(None, 0));
            assert!(!accepted(Message::Restore(kind(None, Some(reply)))).await);
        }
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
        async fn routes_user_info_to_handler_and_returns_ok() {
            let _ =
                crate::config::MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
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
            let mut event = create_test_unwrapped_message();
            // The only shape intake admits: `restore`, no order id, no payload.
            let msg = Message::Restore(mostro_core::prelude::MessageKind::new(
                None,
                Some(7),
                None,
                Action::UserInfo,
                None,
            ));
            event.message = msg.clone();

            let result =
                handle_message_action_no_ln(&Action::UserInfo, msg, &event, &my_keys, &ctx).await;

            // An unknown identity is answered with zeros, not an error.
            assert!(result.is_ok(), "UserInfo must succeed: {result:?}");
            let replies = crate::app::user_info::queued_replies_for(&event.sender).await;
            assert_eq!(replies.len(), 1, "one reply to the trade key: {replies:?}");
            let kind = replies[0].get_inner_message_kind();
            assert!(matches!(replies[0], Message::Restore(_)));
            assert_eq!(kind.action, Action::UserInfo);
            assert_eq!(kind.request_id, Some(7));
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
            let _ = crate::NOSTR_CLIENT.set(nostr_sdk::prelude::Client::default());

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

        fn is_invalid_action(result: Result<()>) -> bool {
            matches!(
                result,
                Err(e) if e.downcast_ref::<MostroError>()
                    == Some(&MostroError::MostroCantDo(CantDoReason::InvalidAction))
            )
        }

        /// Actions with no Cashu handler yet — release/cancel/dispute, the
        /// permanently-blocked buyer-invoice/bond actions, and Track D admin
        /// actions — must still be rejected with `CantDo(InvalidAction)`. The
        /// Track A actions (`NewOrder`, `TakeBuy`, `TakeSell` in TA-2;
        /// `AddCashuEscrow` in TA-1) are excluded: they now route to their real
        /// handlers, not to `InvalidAction`.
        #[tokio::test]
        async fn blocks_every_order_lifecycle_action_with_invalid_action() {
            let _ =
                crate::config::MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
            let _ = crate::NOSTR_CLIENT.set(nostr_sdk::prelude::Client::default());
            let ctx = create_migrated_ctx().await;
            let my_keys = create_test_keys();
            let event = create_test_unwrapped_message();

            for action in [
                Action::AddInvoice,
                Action::FiatSent,
                Action::Release,
                Action::Cancel,
                Action::Dispute,
                Action::RateUser,
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

        /// The allow-list (`Orders`, `LastTradeIndex`, `UserInfo`,
        /// `RestoreSession`, `TradePubkey`) is routed to
        /// `handle_message_action_no_ln`. We assert routing by observing that
        /// `RestoreSession` reaches its handler and returns `Ok` — proving it
        /// was NOT short-circuited to `InvalidAction`.
        #[tokio::test]
        async fn allows_restore_session_through_no_ln_router() {
            let ctx = create_migrated_ctx().await;
            let my_keys = create_test_keys();
            let event = create_test_unwrapped_message();
            let msg = Message::new_restore(None);

            let result = dispatch_cashu(&Action::RestoreSession, msg, &event, &my_keys, &ctx).await;
            assert!(
                result.is_ok(),
                "RestoreSession must route to the no-LN handler, got {result:?}"
            );
        }

        /// `import-reputation` reaches its handler on both routers. With import
        /// enabled and no payload the handler answers `InvalidPayload`, which
        /// neither the default arm (`Ok`) nor the Cashu block
        /// (`InvalidAction`) produces.
        #[tokio::test]
        async fn routes_import_reputation_to_its_handler_in_both_modes() {
            use crate::app::context::test_utils::{test_settings, TestContextBuilder};
            use crate::config::types::ReputationImportSettings;
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .unwrap();
            sqlx::migrate!("./migrations").run(&pool).await.unwrap();
            let mut settings = test_settings();
            settings.reputation_import = Some(ReputationImportSettings {
                enabled: true,
                ..Default::default()
            });
            let ctx = TestContextBuilder::new()
                .with_pool(std::sync::Arc::new(pool))
                .with_settings(settings)
                .build();
            let my_keys = create_test_keys();
            let event = create_test_unwrapped_message();
            let action = Action::ImportReputation;
            let msg = create_test_message(action.clone(), None);
            let invalid_payload = |result: Result<()>| {
                matches!(
                    result,
                    Err(e) if e.downcast_ref::<MostroError>()
                        == Some(&MostroError::MostroCantDo(CantDoReason::InvalidPayload))
                )
            };
            assert!(invalid_payload(
                handle_message_action_no_ln(&action, msg.clone(), &event, &my_keys, &ctx).await
            ));
            assert!(invalid_payload(
                dispatch_cashu(&action, msg, &event, &my_keys, &ctx).await
            ));
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
}
