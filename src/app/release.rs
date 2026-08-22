use crate::app::bond;
use crate::app::context::AppContext;
use crate::app::dispute::close_dispute_after_user_resolution;
use crate::escrow::EscrowBackend;
use crate::lightning::invoice::{decode_invoice, validate_payout_invoice};
use crate::lightning::{LndConnector, PaymentMessage, PAYOUT_SEND_PAYMENT_TIMEOUT};
use crate::lnurl::resolv_ln_address;
use crate::nip33::{new_order_event_with_created_at, order_to_tags};
use crate::util::{
    bytes_to_string, enqueue_order_msg, get_order, mark_orderbook_publish_failed,
    monotonic_order_event_timestamp, settle_seller_hold_invoice, update_order_event,
};
use crate::Result;
use bitcoin::hashes::hex::FromHex;

use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;
use lnurl::lightning_address::LightningAddress;
use lnurl::lnurl::LnUrl;
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use std::cmp::Ordering;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::mpsc::channel;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{info, warn};

/// Cap on concurrently running payout send tasks. Since `do_payment` returns
/// right after claiming, the scheduler retry loop can fan a backlog of N
/// failed payouts into N background tasks; this semaphore makes the sends
/// queue instead of fanning out. It bounds concurrent *payment streams*, NOT
/// LND connections: `LndConnector::new()` runs before the claim (a connect
/// blip must never leave a marker set), so a backlog still opens N
/// connections that sit idle while queued — a deliberate trade for the
/// fail-fast-before-claim property.
///
/// The queue puts an unbounded wait between the claim and the send, with two
/// hazards, both closed by `touch_order_payout_claim`:
/// - **double payout**: reconciliation re-arms the claim and the buyer
///   supplies a fresh invoice under a *different* hash — a case neither the
///   pre-send duplicate guard nor LND's duplicate rejection can catch.
///   Closed by the revalidating touch right after the permit: a task whose
///   claim was re-armed or replaced while it queued drops its send.
/// - **spurious failure**: a queued-but-never-sent payout ages past the
///   reconcile grace window and is re-armed as failed, burning the buyer's
///   retry budget and eventually re-prompting for an invoice that was never
///   needed. Closed by the heartbeat touch while waiting (see
///   [`PAYOUT_QUEUE_HEARTBEAT`]), which keeps the claim younger than grace.
///
/// Fixed for now; could become a settings knob later.
static PAYOUT_DISPATCH_SEMAPHORE: Semaphore = Semaphore::const_new(8);

/// Refresh cadence for a payout claim while its task waits for a send
/// permit. Derived from — and strictly below — the reconciler's minimum
/// grace window, so a queued claim is always re-stamped before it becomes
/// eligible for reconciliation.
const PAYOUT_QUEUE_HEARTBEAT: Duration =
    Duration::from_secs(crate::scheduler::MIN_GRACE_SECS as u64 * 2 / 3);

/// Run [`check_failure_retries`] and surface bookkeeping failures instead of
/// silently dropping them. On success, preserves the existing retry-count log.
async fn check_failure_retries_or_log(ctx: &AppContext, order: &Order, request_id: Option<u64>) {
    match check_failure_retries(ctx, order, request_id).await {
        Ok(failed_payment) => {
            info!(
                "Order id {} has {} failed payments retries",
                failed_payment.id, failed_payment.payment_attempts
            );
        }
        Err(e) => {
            warn!(
                "Order id {}: check_failure_retries failed: {:?}",
                order.id, e
            );
        }
    }
}

/// Check if order has failed payment retries
pub async fn check_failure_retries(
    ctx: &AppContext,
    order: &Order,
    request_id: Option<u64>,
) -> Result<Order, MostroError> {
    let mut order = order.clone();

    let pool = ctx.pool();

    // Get max number of retries
    let ln_settings = &ctx.settings().lightning;
    let retries_number = ln_settings.payment_attempts as i64;

    let is_first_failure = order.payment_attempts == 0;

    // Count payment retries up to limit
    order.count_failed_payment(retries_number);

    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Only send notification on first failure
    if is_first_failure {
        // Create payment failed payload with retry configuration
        let payment_failed_info = PaymentFailedInfo {
            payment_attempts: ln_settings.payment_attempts.saturating_sub(1),
            payment_retries_interval: ln_settings.payment_retries_interval,
        };

        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::PaymentFailed,
            Some(Payload::PaymentFailed(payment_failed_info)),
            buyer_pubkey,
            None,
        )
        .await;
    } else if order.payment_attempts >= retries_number {
        // Clone order
        let mut order_payment_failed = order.clone();
        // Update amount notified to the buyer (only Mostro fee, not dev fee)
        order_payment_failed.amount = order_payment_failed.amount.saturating_sub(order.fee);
        if order_payment_failed.amount <= 0 {
            return Err(MostroCantDo(CantDoReason::InvalidAmount));
        }
        // Check errors
        if mostro_core::order::Kind::from_str(&order.kind).is_err() {
            return Err(MostroCantDo(CantDoReason::InvalidOrderKind));
        }
        // Check status
        if order_payment_failed.get_order_status().is_err() {
            return Err(MostroInternalErr(ServiceError::InvalidOrderStatus));
        }

        // Send message to buyer indicating payment failed
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::AddInvoice,
            Some(Payload::Order(SmallOrder::from(
                order_payment_failed.clone(),
            ))),
            buyer_pubkey,
            None,
        )
        .await;
    }

    // Only update payment-retry fields to avoid overwriting fields modified by
    // concurrent processes (dev_fee_paid, dev_fee_payment_hash, status, etc.)
    sqlx::query("UPDATE orders SET failed_payment = ?, payment_attempts = ? WHERE id = ?")
        .bind(order.failed_payment)
        .bind(order.payment_attempts)
        .bind(order.id)
        .execute(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(order)
}

/// Handles the release action for an order, managing the release of funds and subsequent order flow.
///
/// This function is responsible for processing the release of funds in a trade, which is a critical
/// step in the order lifecycle. It verifies the seller's identity, manages the settlement of hold
/// invoices, and coordinates the creation of child orders for range orders. The function also
/// handles notifications to both buyer and seller about the release status.
///
/// # Arguments
///
/// * `ctx` - Application context containing the database pool and other dependencies
/// * `msg` - The message containing the release request and associated metadata
/// * `event` - The unwrapped gift event containing the seller's signature and verification data
/// * `my_keys` - The Mostro node's keys used for signing events and messages
/// * `escrow` - Escrow backend used to settle the held funds (Lightning
///   hold-invoice pass-through today; see `crate::escrow`)
///
/// # Returns
///
/// Returns a `Result<(), MostroError>` where:
/// * `Ok(())` indicates successful release of funds and order processing
/// * `Err(MostroError)` indicates an error occurred during the process
///
/// # Flow
///
/// 1. Validates the request:
///    - Verifies the seller's identity matches the order
///    - Checks if the order status allows for release
///
/// 2. Processes the release:
///    - Settles the seller's hold invoice
///    - Updates the order status to SettledHoldInvoice
///    - Notifies the buyer about the release
///
/// 3. Handles child orders (for range orders):
///    - Creates and processes child orders if applicable
///    - Sends notifications to next traders in the sequence
///
/// 4. Sends notifications:
///    - Notifies seller about hold invoice settlement
///    - Requests rating from seller
///    - Initiates payment to buyer
///
/// # Errors
///
/// This function may return the following errors:
/// * `MostroCantDo(CantDoReason::InvalidPeer)` - If the seller's identity doesn't match
/// * `MostroCantDo(CantDoReason::NotAllowedByStatus)` - If the order status doesn't allow release
/// * `MostroInternalErr(ServiceError::DbAccessError)` - If database operations fail
/// * `MostroInternalErr(ServiceError::NostrError)` - If there are issues with Nostr operations
/// * `MostroInternalErr(ServiceError::InvoiceInvalidError)` - If there are issues with the invoice
///
/// # Security Considerations
///
/// * Only the seller can release funds for their order
/// * The seller's identity is verified through the event signature
/// * Hold invoices are settled only after proper verification
pub async fn release_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
    escrow: &mut dyn EscrowBackend,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let mut order = get_order(&msg, pool).await?;
    // Get seller pubkey hex
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    // We send a message to buyer indicating seller released funds
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Check if the pubkey is the seller pubkey - Only the seller can release funds
    if seller_pubkey != event.sender {
        return Err(MostroCantDo(CantDoReason::InvalidPeer));
    }

    // Check if order is in status fiat sent or dispute
    if order.check_status(Status::FiatSent).is_err() && order.check_status(Status::Dispute).is_err()
    {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    // Get next trade key
    let next_trade = msg
        .get_inner_message_kind()
        .get_next_trade_key()
        .map_err(MostroInternalErr)?;

    // Settle seller hold invoice
    settle_seller_hold_invoice(event, escrow, Action::Released, false, &order).await?;
    // Update order event with status SettledHoldInvoice
    order = update_order_event(my_keys, Status::SettledHoldInvoice, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Persist the status change to DB before calling do_payment.
    // do_payment spawns async tasks that capture an Order copy; without this
    // explicit write the settled-hold-invoice status only lived in memory and
    // was persisted as a side-effect of the full-row writes in
    // check_failure_retries / payment_success (now replaced by targeted updates).
    let result =
        sqlx::query("UPDATE orders SET status = ?, event_id = ? WHERE id = ? AND status IN (?, ?)")
            .bind(&order.status)
            .bind(&order.event_id)
            .bind(order.id)
            .bind(Status::FiatSent.to_string())
            .bind(Status::Dispute.to_string())
            .execute(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if result.rows_affected() == 0 {
        tracing::warn!(
            "Order {} not transitioned to settled-hold-invoice: status changed concurrently",
            order.id
        );
        return Ok(());
    }

    // If there was an active dispute on this order, close it since the seller
    // released the funds, resolving the situation.
    close_dispute_after_user_resolution(ctx, &order, DisputeStatus::Settled, my_keys, "release")
        .await;

    enqueue_order_msg(
        None,
        Some(order.id),
        Action::Released,
        None,
        buyer_pubkey,
        None,
    )
    .await;

    // Handle child order for range orders. A spawned remainder means the
    // range continues, so the maker stays committed and its bond stays
    // `Locked`. No remainder means the range is fully consumed (or this was
    // a fixed-amount order) — resolve the maker bond at close (Phase 6
    // settle-at-close, or the Phase 5 release for a non-range maker bond).
    match get_child_order(ctx, order.clone(), my_keys).await {
        Ok((Some(child_order), Some(event))) => {
            let child_order_id = child_order.id;
            // The hold invoice is already settled at this point, so a child
            // failure must never abort the release: skip the remainder,
            // resolve the maker bond and continue to the buyer payout. The
            // child order is persisted before its event is published so a
            // persistence failure never leaves a ghost order on the book.
            match handle_child_order(child_order, &order, next_trade, ctx.pool(), request_id).await
            {
                Ok(()) => {
                    let client = ctx.nostr_client();
                    // A per-relay rejection resolves to `Ok` with the
                    // refusing relays in `output.failed`; both that and a
                    // full send error leave the book divergent somewhere,
                    // so queue the child for the reconciler.
                    match client.send_event(&event).await {
                        Ok(output) if output.failed.is_empty() => {}
                        Ok(output) => {
                            tracing::warn!(
                                "child order event rejected by {} relay(s) for order id: {}: {:?}; queued for republish by the orderbook reconciler",
                                output.failed.len(),
                                child_order_id,
                                output.failed
                            );
                            mark_orderbook_publish_failed(child_order_id);
                        }
                        Err(_) => {
                            tracing::warn!("Failed sending child order event for order id: {}; queued for republish by the orderbook reconciler", child_order_id);
                            mark_orderbook_publish_failed(child_order_id);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        order_id = %order.id,
                        error = %e,
                        "handle_child_order failed (e.g. Release without NextTrade); skipping remainder, resolving maker bond and continuing with buyer payout"
                    );
                    bond::resolve_range_maker_bond_at_close_or_warn(pool, &order, "release_action")
                        .await;
                }
            }
        }
        Ok(_) => {
            bond::resolve_range_maker_bond_at_close_or_warn(pool, &order, "release_action").await;
        }
        Err(e) => {
            // `get_child_order` only *computes* the remainder (it neither
            // persists nor publishes a child), so on error no remainder
            // exists on the book — the range has effectively ended. Resolve
            // the maker bond at close rather than leaving it Locked until
            // the LND CLTV safety net. (mostro does not retry child-order
            // creation anywhere; the lost remainder is a pre-existing
            // limitation, logged here.)
            tracing::warn!(
                order_id = %order.id,
                error = %e,
                "get_child_order failed; resolving maker bond at close (no remainder was created)"
            );
            bond::resolve_range_maker_bond_at_close_or_warn(pool, &order, "release_action").await;
        }
    }

    // We send a HoldInvoicePaymentSettled message to seller, the client should
    // indicate *funds released* message to seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::HoldInvoicePaymentSettled,
        None,
        seller_pubkey,
        None,
    )
    .await;

    // We send a message to seller indicating seller released funds
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::Rate,
        None,
        seller_pubkey,
        None,
    )
    .await;

    // Phase 1/6: release the taker bond(s) on this slice. The maker bond is
    // handled by `resolve_range_maker_bond_at_close_or_warn` above — its
    // single HTLC may span a whole range, so releasing it here would
    // wrongly cancel a bond still committed to a continuing range. A failed
    // bond release is logged but does not block trade finalization.
    bond::release_taker_bonds_for_order_or_warn(pool, order.id, "release_action").await;

    // Finally we try to pay buyer's invoice
    let _ = do_payment(ctx, order, request_id).await;

    Ok(())
}

/// Helper function to handle buy order case in child order creation
fn handle_buy_child_order(
    child_order: &mut Order,
    order: &Order,
    normal_buyer_idkey: Option<String>,
) -> Result<(Option<String>, Option<i64>), MostroError> {
    let next_buyer_pubkey = order.next_trade_pubkey.clone().ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(
            "Next trade buyer pubkey is missing".to_string(),
        ))
    })?;

    child_order.buyer_pubkey = Some(next_buyer_pubkey.clone());
    child_order.trade_index_buyer = order.next_trade_index;
    child_order.creator_pubkey = next_buyer_pubkey.clone();
    // if user is in full privacy mode, use the next trade key
    // if user is in normal mode, use the master buyer pubkey
    match normal_buyer_idkey {
        Some(idkey) => {
            child_order.master_buyer_pubkey = Some(idkey);
        }
        None => {
            child_order.master_buyer_pubkey = Some(next_buyer_pubkey);
        }
    }

    Ok((
        child_order.buyer_pubkey.clone(),
        child_order.trade_index_buyer,
    ))
}

/// Helper function to handle sell order case in child order creation
fn handle_sell_child_order(
    child_order: &mut Order,
    next_trade: Option<(String, u32)>,
    normal_seller_idkey: Option<String>,
) -> Result<(Option<String>, Option<i64>), MostroError> {
    let (next_trade_pubkey, next_trade_index) = next_trade.ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(
            "Next trade seller pubkey is missing".to_string(),
        ))
    })?;

    let next_trade_pubkey = PublicKey::from_str(&next_trade_pubkey)
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;

    child_order.seller_pubkey = Some(next_trade_pubkey.to_string());
    child_order.trade_index_seller = Some(next_trade_index as i64);
    child_order.creator_pubkey = next_trade_pubkey.to_string();
    // if user is in full privacy mode, use the next trade key as master seller pubkey
    // if user is in normal mode, use the master seller pubkey as master seller pubkey
    match normal_seller_idkey {
        Some(idkey) => {
            child_order.master_seller_pubkey = Some(idkey);
        }
        None => {
            child_order.master_seller_pubkey = Some(next_trade_pubkey.to_string());
        }
    }

    Ok((
        child_order.seller_pubkey.clone(),
        child_order.trade_index_seller,
    ))
}

/// Manages the creation and update of child orders in a range order sequence.
///
/// This function handles the creation and setup of child orders for range orders, which are orders
/// that can be split into multiple smaller orders. It manages assignment of pubkeys, sets up
/// trade indices, and handles notifications to the next trader in the sequence.
///
/// # Arguments
///
/// * `child_order` - The child order to be created/updated. This is a new order derived from the parent order.
/// * `order` - The parent order from which the child order is derived. Contains the original order details.
/// * `next_trade` - Optional tuple containing the next trader's information:
///   - First element: The public key of the next trader
///   - Second element: The trade index for the next trade
/// * `pool` - Database connection pool for storing the child order
/// * `request_id` - Optional request ID used for message queuing and tracking
///
/// # Returns
///
/// Returns a `Result<(), MostroError>` where:
/// * `Ok(())` indicates successful creation and setup of the child order
/// * `Err(MostroError)` indicates an error occurred during the process
///
/// # Flow
///
/// 1. Determines if users are in rating mode or full privacy mode
/// 2. Based on order type (buy/sell):
///    - For buy orders: Sets up buyer-specific fields and assigns buyer pubkey
///    - For sell orders: Sets up seller-specific fields and assigns seller pubkey
/// 3. Creates a new pending child order
/// 4. If next trade information is available:
///    - Enqueues a notification message to the next trader
/// 5. Stores the child order in the database
///
/// # Errors
///
/// This function may return the following errors:
/// * `MostroInternalErr(ServiceError::UnexpectedError)` - If the order type or creator is invalid
/// * `MostroInternalErr(ServiceError::DbAccessError)` - If database operations fail
/// * `MostroInternalErr(ServiceError::NostrError)` - If there are issues with Nostr operations
async fn handle_child_order(
    mut child_order: Order,
    order: &Order,
    next_trade: Option<(String, u32)>,
    pool: &Pool<Sqlite>,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    // Check if users are in rating mode or full privacy mode - if a key is Some the user in in normal mode
    // if a key is None the user is in full privacy mode
    let (normal_buyer_idkey, normal_seller_idkey) =
        order.is_full_privacy_order().map_err(|_| {
            MostroInternalErr(ServiceError::UnexpectedError(
                "Error creating order event".to_string(),
            ))
        })?;

    let (notification_pubkey, new_trade_index) = if order.is_buy_order().is_ok()
        && order.buyer_pubkey.as_ref() == Some(&order.creator_pubkey)
    {
        handle_buy_child_order(&mut child_order, order, normal_buyer_idkey)?
    } else if order.is_sell_order().is_ok()
        && order.seller_pubkey.as_ref() == Some(&order.creator_pubkey)
    {
        handle_sell_child_order(&mut child_order, next_trade, normal_seller_idkey)?
    } else {
        return Err(MostroInternalErr(ServiceError::UnexpectedError(
            "Invalid order type or creator".to_string(),
        )));
    };

    // Prepare new pending child order
    let new_order = child_order.as_new_order();

    // Validate the notification data before touching the database
    let Some(destination_pubkey) = notification_pubkey else {
        return Err(MostroInternalErr(ServiceError::UnexpectedError(
            "Next trade index or pubkey is missing - user cannot be notified".to_string(),
        )));
    };
    let destination_pubkey = PublicKey::from_str(&destination_pubkey)
        .map_err(|_| MostroInternalErr(ServiceError::NostrError("Invalid pubkey".to_string())))?;

    // Create the child order in database before queueing its notification,
    // so the queue never delivers a NewOrder message for a row that does
    // not exist (e.g. on a transient insert failure).
    child_order
        .create(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    enqueue_order_msg(
        request_id,
        new_order.id,
        Action::NewOrder,
        Some(Payload::Order(new_order)),
        destination_pubkey,
        new_trade_index,
    )
    .await;

    Ok(())
}

/// Dispatch the buyer payout for a settled-hold-invoice order.
///
/// `Ok(())` means the payout was *dispatched* (or a claim already exists),
/// not that it settled: everything up to and including the idempotency claim
/// runs inline — bounded work only — and the `send_payment` call itself runs
/// in a background task so a payment that never reaches a terminal state
/// (hold invoice, HTLC stuck in route) cannot freeze the event loop. The
/// task is bounded by [`PAYOUT_SEND_PAYMENT_TIMEOUT`]; on timeout the claim
/// marker is kept and `reconcile_inflight_payout` owns the outcome.
///
/// Lightning Addresses **and** bech32 LNURLs are resolved via
/// [`resolv_ln_address`] under the LNURL host policy. A non-empty `pr` must
/// decode as BOLT11 and pass [`validate_payout_invoice`] (chain match, final
/// CLTV bound, not already expired) before LND submission; resolve, decode
/// and validation failures — all pre-claim — go through
/// [`check_failure_retries_or_log`] and return `Err`. `send_payment` RPC
/// errors and streamed `PaymentStatus::Failed` updates bump the same retry
/// bookkeeping from the background task. Callers such as `release_action`
/// ignore the result after hold settlement — retries are driven by the
/// failed-payment job.
pub async fn do_payment(
    ctx: &AppContext,
    order: Order,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    let payment_request = match order.buyer_invoice.as_ref() {
        Some(req) => req.to_string(),
        _ => return Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
    };

    let ln_addr = LightningAddress::from_str(&payment_request);
    // Calculate buyer's portion after subtracting only the Mostro fee
    // Dev fee is NOT charged to buyer - it's paid by mostrod from its earnings
    let amount = (order.amount as u64).saturating_sub(order.fee as u64);
    if amount == 0 {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }
    // `is_valid_invoice` accepts a Lightning Address *and* an encoded LNURL, so
    // both have to be resolved to a BOLT11 invoice here. An unresolved LNURL
    // reaches `send_payment`, fails to decode and burns the retry budget
    // instead of ever paying the buyer.
    let payout_destination = match &ln_addr {
        Ok(addr) => Some(addr.to_string()),
        Err(_) if LnUrl::from_str(&payment_request).is_ok() => Some(payment_request.clone()),
        Err(_) => None,
    };

    let payment_request = match payout_destination {
        // Resolving a payout destination is a network round-trip to a host the
        // buyer chose. When it yields no usable invoice — forbidden host (SSRF
        // policy), unreachable, LNURL-level ERROR, malformed or unpayable `pr`
        // — that is a payment failure and must go through the same bookkeeping
        // as a failed `send_payment`. Returning early would leave
        // `failed_payment = false` and hide the order from the retry job.
        Some(destination) => match resolv_ln_address(&destination, amount, None).await {
            Ok(pr) if !pr.is_empty() => match decode_invoice(&pr) {
                // The LNURL server picked this invoice, not the buyer, so it
                // never went through `validate_invoice`. Apply the payout rules
                // before handing it to LND.
                Ok(invoice) => match validate_payout_invoice(&invoice) {
                    Ok(()) => pr,
                    Err(e) => {
                        warn!(
                            "Order id {}: payout address returned unpayable invoice: {:?}",
                            order.id, e
                        );
                        check_failure_retries_or_log(ctx, &order, request_id).await;
                        return Err(e);
                    }
                },
                Err(e) => {
                    warn!(
                        "Order id {}: payout address returned malformed invoice: {:?}",
                        order.id, e
                    );
                    check_failure_retries_or_log(ctx, &order, request_id).await;
                    return Err(MostroInternalErr(ServiceError::LnAddressParseError));
                }
            },
            outcome => {
                match outcome {
                    Err(e) => warn!(
                        "Order id {}: could not resolve payout address: {:?}",
                        order.id, e
                    ),
                    _ => warn!("Order id {}: payout address returned no invoice", order.id),
                }
                check_failure_retries_or_log(ctx, &order, request_id).await;
                return Err(MostroInternalErr(ServiceError::LnAddressParseError));
            }
        },
        None => payment_request,
    };

    // Resolve the buyer pubkey *before* claiming: a malformed order fails
    // here without a claim, so no marker is ever left set for a payout that
    // was never dispatched.
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Connect to LND *before* claiming: if the connection fails, `?` returns
    // here without a claim, so a transient connect blip never leaves a marker
    // set with no payment behind it.
    let mut ln_client_payment = LndConnector::new().await?;

    // Idempotency claim: persist the payout invoice's `payment_hash` (and the
    // claim timestamp) immediately before dispatch. While the marker is set,
    // `find_failed_payment` skips this order and `pay_new_invoice` rejects
    // invoice swaps, so no second payout can be dispatched for the same settled
    // escrow. This CAS also loses to a concurrent claim (two scheduler ticks
    // racing) — only the winner pays. Cleared on a confirmed-terminal outcome or
    // by reconciliation; the timestamp keeps reconciliation from acting on this
    // payout until LND has surely registered it (closing the reconcile-vs-send
    // race). The dispatch task re-validates and refreshes this claim
    // (`touch_order_payout_claim`) after its semaphore wait, so the window
    // between the (refreshed) claim and LND registering the payment is only
    // the send call itself even when the task queued behind a backlog.
    let payout_hash = decode_invoice(&payment_request)
        .map(|inv| bytes_to_string(inv.payment_hash().as_ref()))
        .map_err(|_| MostroInternalErr(ServiceError::InvoiceInvalidError))?;
    let Some(payout_claimed_at) =
        crate::db::claim_order_payout(ctx.pool(), order.id, &payout_hash).await?
    else {
        warn!(
            "Order {}: a payout is already in flight (or status changed); skipping duplicate send_payment",
            order.id
        );
        return Ok(());
    };

    // Get Mostro keys from context
    let my_keys = ctx.keys().clone();

    // Clone ctx for the background task
    let ctx = ctx.clone();

    // From here on the payout runs OFF the event loop: `send_payment` waits
    // for LND's payment stream to reach a terminal state, which a locked-in
    // but unresolved HTLC (hold invoice, HTLC stuck in route) can delay
    // indefinitely — awaiting it inline froze the whole daemon. The claim
    // persisted above is what makes backgrounding safe: whatever happens to
    // this task (RPC error, timeout, process restart), reconciliation can
    // always finish or fail the payout by hash, so it is never lost or paid
    // twice.
    tokio::spawn(async move {
        // The claim token; refreshed by every successful touch below.
        let mut payout_claimed_at = payout_claimed_at;

        // Bound concurrent sends (see PAYOUT_DISPATCH_SEMAPHORE). While
        // queued, heartbeat the claim: re-validate and re-stamp it every
        // PAYOUT_QUEUE_HEARTBEAT so it never ages past the reconcile grace
        // window — without this, reconciliation would treat a queued-but-
        // never-sent payout as failed, burning the buyer's retry budget for
        // a payment that was never attempted. The acquire future is pinned
        // OUTSIDE the loop so the task keeps its FIFO position in the
        // semaphore queue across heartbeats. The semaphore is static and
        // never closed, so acquire() only errs if it were closed; proceeding
        // unpermitted in that impossible case beats silently dropping a
        // claimed payout. The permit is then held for the send and the
        // RPC-error reconcile via RAII — the watcher is a sibling task and
        // finishes its bookkeeping outside the bound.
        let acquire = PAYOUT_DISPATCH_SEMAPHORE.acquire();
        tokio::pin!(acquire);
        let _permit = loop {
            tokio::select! {
                permit = &mut acquire => break permit,
                _ = tokio::time::sleep(PAYOUT_QUEUE_HEARTBEAT) => {
                    match crate::db::touch_order_payout_claim(
                        ctx.pool(),
                        order.id,
                        &payout_hash,
                        Some(payout_claimed_at),
                    )
                    .await
                    {
                        Ok(Some(refreshed_at)) => payout_claimed_at = refreshed_at,
                        Ok(None) => {
                            warn!(
                                "Order {}: payout claim was re-armed or replaced while queued; dropping stale dispatch of hash {}",
                                order.id, payout_hash
                            );
                            return;
                        }
                        Err(e) => {
                            warn!(
                                "Order {}: could not heartbeat payout claim while queued ({e}); dropping dispatch of hash {} — reconciliation will resolve the kept marker",
                                order.id, payout_hash
                            );
                            return;
                        }
                    }
                }
            }
        };

        // Final re-validation now that the queue wait is over, refreshing
        // the timestamp in the same CAS (the last heartbeat may be up to a
        // full PAYOUT_QUEUE_HEARTBEAT old). If reconciliation re-armed the
        // claim while this task queued (the buyer may already have supplied
        // a fresh invoice under a different hash), a newer payout owns the
        // order and this invoice must NOT be sent. On a DB error, dropping
        // the send is also the safe direction: the kept marker is
        // recoverable by reconciliation, a blind send is not. The refreshed
        // timestamp is the claim token from here on — it restarts the
        // reconcile grace clock and invalidates any pre-touch snapshot a
        // reconciler already holds.
        payout_claimed_at = match crate::db::touch_order_payout_claim(
            ctx.pool(),
            order.id,
            &payout_hash,
            Some(payout_claimed_at),
        )
        .await
        {
            Ok(Some(refreshed_at)) => refreshed_at,
            Ok(None) => {
                warn!(
                    "Order {}: payout claim was re-armed or replaced while queued; dropping stale dispatch of hash {}",
                    order.id, payout_hash
                );
                return;
            }
            Err(e) => {
                warn!(
                    "Order {}: could not re-validate payout claim after queue ({e}); dropping dispatch of hash {} — reconciliation will resolve the kept marker",
                    order.id, payout_hash
                );
                return;
            }
        };

        let (tx, mut rx) = channel::<PaymentMessage>(100);

        // Start the status watcher BEFORE `send_payment` and run them
        // concurrently: `send_payment` forwards every LND update through `tx`
        // and blocks when the channel fills, so a watcher started only after
        // it returns could deadlock the payment on a chatty stream. The
        // watcher ends on its own when `tx` drops (send_payment returned or
        // its future was dropped by the timeout).
        let watcher = {
            let ctx = ctx.clone();
            let payout_hash = payout_hash.clone();
            let mut order = order.clone();
            async move {
                // Receiving msgs from send_payment()
                while let Some(msg) = rx.recv().await {
                    if let Ok(status) = PaymentStatus::try_from(msg.payment.status) {
                        match status {
                            PaymentStatus::Succeeded => {
                                info!(
                                    "Order Id {}: Invoice with hash: {} paid!",
                                    order.id, msg.payment.payment_hash
                                );
                                // Release our claim only if the order actually
                                // reached Success. If finalization fails, keep the
                                // marker so reconciliation retries it — clearing it
                                // here would strand a paid order with no recovery.
                                if payment_success(
                                    &ctx,
                                    &mut order,
                                    buyer_pubkey,
                                    &my_keys,
                                    request_id,
                                )
                                .await
                                .unwrap_or(false)
                                {
                                    let _ = crate::db::clear_order_payout(
                                        ctx.pool(),
                                        order.id,
                                        &payout_hash,
                                        Some(payout_claimed_at),
                                    )
                                    .await;
                                }
                            }
                            PaymentStatus::Failed => {
                                warn!(
                                    "Order Id {}: Invoice with hash: {} has failed!",
                                    order.id, msg.payment.payment_hash
                                );

                                // Release our own claim (scoped to this hash and
                                // the per-claim timestamp) and re-arm retry. Only
                                // do the failure bookkeeping and buyer
                                // notification if we still owned the claim, so a
                                // stale watcher never pollutes a newer payout's
                                // retry state or notifies against it — even when
                                // the retry reused the same invoice/hash.
                                if crate::db::fail_order_payout(
                                    ctx.pool(),
                                    order.id,
                                    &payout_hash,
                                    Some(payout_claimed_at),
                                )
                                .await
                                .unwrap_or(false)
                                {
                                    check_failure_retries_or_log(&ctx, &order, request_id).await;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        };
        tokio::spawn(watcher);

        let send_outcome = timeout(
            PAYOUT_SEND_PAYMENT_TIMEOUT,
            ln_client_payment.send_payment(&payment_request, amount as i64, tx),
        )
        .await;

        // The status lookup is only meaningful after an RPC-level send
        // failure: it is what decides between keeping the marker and
        // re-arming. `None` alongside an Ok(Err) send means the hash was
        // unusable (should not happen — it was built from the invoice).
        let lookup = match &send_outcome {
            Ok(Err(_)) => match Vec::<u8>::from_hex(&payout_hash) {
                Ok(bytes) if bytes.len() == 32 => {
                    Some(ln_client_payment.lookup_payment_status(&bytes).await)
                }
                _ => None,
            },
            _ => None,
        };

        match classify_dispatch(send_outcome, lookup) {
            // The send stream ended. Usually a terminal update was delivered
            // and the watcher finishes the bookkeeping — but send_payment's
            // `while let Ok(Some(..))` swallows a mid-stream gRPC error, so
            // this also covers a stream that died or EOF'd with no terminal
            // update. In that case the watcher exits without acting, the
            // claim marker stays set, and reconciliation resolves the real
            // outcome by payment hash.
            DispatchVerdict::StreamEnded => {}
            DispatchVerdict::KeepMarker(cause) => {
                warn!(
                    "Order Id {}: keeping payout claim for hash {}: {cause}",
                    order.id, payout_hash
                );
            }
            DispatchVerdict::ReArm(cause) => {
                warn!(
                    "Order Id {}: payout dispatch failed ({cause}); re-arming retry for hash {}",
                    order.id, payout_hash
                );
                if crate::db::fail_order_payout(
                    ctx.pool(),
                    order.id,
                    &payout_hash,
                    Some(payout_claimed_at),
                )
                .await
                .unwrap_or(false)
                {
                    check_failure_retries_or_log(&ctx, &order, request_id).await;
                }
            }
        }
    });

    Ok(())
}

/// What the dispatch task must do with its claim once the bounded send has
/// ended (see `do_payment`).
#[derive(Debug, PartialEq)]
enum DispatchVerdict {
    /// The send stream ended; the watcher owns any bookkeeping. Nothing to
    /// do with the claim here.
    StreamEnded,
    /// KEEP the claim marker (with this cause): the payment may still
    /// settle, so reconciliation owns the outcome and no second payout is
    /// ever dispatched.
    KeepMarker(String),
    /// Release the claim (scoped to hash + token) and re-arm retry now, with
    /// this cause: LND confirms nothing is or will be in flight for it.
    ReArm(String),
}

/// Classify the outcome of the bounded `send_payment` into what happens to
/// the payout claim. Pure — the LND status lookup is a parameter — so the
/// central safety invariant of the background dispatch ("a timed-out send
/// keeps the claim") is under test rather than under a comment.
///
/// - A timed-out send KEEPS the marker: dropping the send future closes our
///   side of the gRPC stream but does NOT cancel the payment — a locked-in
///   HTLC cannot be cancelled by the sender and may still settle later (up
///   to its CLTV). Re-arming against it risks a double payout; kept, the
///   payout is delayed by at most the reconciler cadence, never lost.
/// - An RPC-level send failure resolves the claim by what LND reports for
///   the hash: in flight / settled / lookup error → KEEP (the payment may
///   still settle); failed / unknown / no record / unusable hash → re-arm
///   retry now (and notify the buyer) instead of waiting for the
///   grace-delayed reconciliation job.
fn classify_dispatch(
    send_outcome: Result<Result<(), MostroError>, tokio::time::error::Elapsed>,
    lookup: Option<Result<Option<PaymentStatus>, MostroError>>,
) -> DispatchVerdict {
    match send_outcome {
        Ok(Ok(())) => DispatchVerdict::StreamEnded,
        Err(_) => DispatchVerdict::KeepMarker(format!(
            "no terminal state after {}s; a locked-in HTLC cannot be cancelled by the sender and may still settle — reconciliation will resolve it",
            PAYOUT_SEND_PAYMENT_TIMEOUT.as_secs()
        )),
        Ok(Err(send_err)) => match lookup {
            Some(Ok(Some(PaymentStatus::InFlight))) | Some(Ok(Some(PaymentStatus::Succeeded))) => {
                DispatchVerdict::KeepMarker(format!(
                    "send errored ({send_err}) but LND reports the payment in flight or settled"
                ))
            }
            Some(Err(lookup_err)) => DispatchVerdict::KeepMarker(format!(
                "send errored ({send_err}) and the status lookup failed ({lookup_err}); the payment may still settle"
            )),
            // Failed / Unknown / no record — or an unusable hash (None),
            // which cannot confirm an in-flight payment: nothing to wait
            // for.
            Some(Ok(_)) | None => DispatchVerdict::ReArm(format!("{send_err}")),
        },
    }
}

/// Finalize a paid order: transition `settled-hold-invoice` → `Success` and
/// notify the buyer, but only after the status CAS actually commits.
///
/// Returns `Ok(true)` when the order is now terminal — this call committed the
/// transition, or a concurrent task already did — meaning the caller may safely
/// release the payout marker. Returns `Ok(false)` when the transition could not
/// be built/persisted (`update_order_event` failed): the caller must KEEP the
/// marker so reconciliation retries finalization, otherwise the buyer would be
/// paid on an order stranded in `settled-hold-invoice` with no recovery hook.
///
/// Buyer notifications (`PurchaseCompleted`, `Rate`) are enqueued only after a
/// successful commit, so a retried finalization never spams the buyer.
async fn payment_success(
    ctx: &AppContext,
    order: &mut Order,
    buyer_pubkey: PublicKey,
    my_keys: &Keys,
    request_id: Option<u64>,
) -> Result<bool> {
    let pool = ctx.pool();

    let order_updated = match update_order_event(my_keys, Status::Success, order).await {
        Ok(updated) => updated,
        // Could not build/publish the Success event: leave the order in
        // settled-hold-invoice and signal "not finalized" so the caller keeps
        // the marker for reconciliation.
        Err(_) => return Ok(false),
    };

    // Only update status and event_id to avoid overwriting fields modified by
    // concurrent processes (dev_fee_paid, dev_fee_payment_hash, etc.)
    // The WHERE guard prevents double success transitions from concurrent tasks.
    let result =
        sqlx::query("UPDATE orders SET status = ?, event_id = ? WHERE id = ? AND status = ?")
            .bind(&order_updated.status)
            .bind(&order_updated.event_id)
            .bind(order_updated.id)
            .bind(Status::SettledHoldInvoice.to_string())
            .execute(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if result.rows_affected() == 0 {
        // Another task already finalized this order: it is terminal, so the
        // caller may release the marker, but the notifications were already
        // sent by that task — do not duplicate them.
        tracing::warn!(
            "Order {} not transitioned to success: already processed by another task",
            order_updated.id
        );
        return Ok(true);
    }

    // Committed by us — notify the buyer now.
    enqueue_order_msg(
        None,
        Some(order_updated.id),
        Action::PurchaseCompleted,
        None,
        buyer_pubkey,
        None,
    )
    .await;
    enqueue_order_msg(
        request_id,
        Some(order_updated.id),
        Action::Rate,
        None,
        buyer_pubkey,
        None,
    )
    .await;
    Ok(true)
}

/// The one LND capability `reconcile_inflight_payout` needs: query a payment's
/// status by hash. Behind a trait (mirroring [`crate::app::cancel`]'s
/// `CancelLightning`) so the reconcile branches are unit-testable with a stub
/// instead of a live node.
pub trait PayoutStatusLookup {
    fn lookup_payment_status<'a>(
        &'a mut self,
        payment_hash: &'a [u8],
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<PaymentStatus>, MostroError>>
                + Send
                + 'a,
        >,
    >;
}

impl PayoutStatusLookup for LndConnector {
    fn lookup_payment_status<'a>(
        &'a mut self,
        payment_hash: &'a [u8],
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<PaymentStatus>, MostroError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { LndConnector::lookup_payment_status(self, payment_hash).await })
    }
}

/// Reconcile a single in-flight buyer payout against LND.
///
/// Called by the scheduler for every order whose `payout_payment_hash` is set.
/// This is the counterpart to `do_payment`'s in-process status watcher: it
/// resolves the durable marker for payouts whose watcher never delivered a
/// terminal update — a held/stranded HTLC, or a payout whose watcher task was
/// lost across a restart. Without it, such an order would stay locked forever
/// (regressing the do-payment-stuck bug); with it, the marker is authoritative
/// only until LND confirms the real outcome:
///
/// - `Succeeded` → finalize as `Success` (idempotent via the status CAS) and
///   clear the marker.
/// - `Failed` / `Unknown` / not found → clear the marker, re-arm retry, and run
///   the same failure bookkeeping as the in-process watcher (advance
///   `payment_attempts`, notify the buyer). Re-arming is safe because LND itself
///   rejects a second `SendPaymentV2` for a payment hash it already has in
///   flight or settled, so a fresh dispatch of the same invoice cannot
///   double-pay.
/// - `InFlight` → leave as is; the payout is genuinely pending.
///
/// `payout_claimed_at` is the per-claim token observed for this marker; every
/// release is scoped to it so a claim replaced between the snapshot and here is
/// never clobbered.
pub async fn reconcile_inflight_payout(
    ctx: &AppContext,
    ln_client: &mut impl PayoutStatusLookup,
    order_id: uuid::Uuid,
    payout_payment_hash: &str,
    payout_claimed_at: Option<i64>,
) -> Result<(), MostroError> {
    let pool = ctx.pool();

    // A payment hash is exactly 32 bytes (64 hex chars). Decode with the same
    // `FromHex` used across the codebase and length-check it; a bad-hex or
    // wrong-length marker is corrupt, so treat it as malformed and re-arm rather
    // than sending a truncated hash to LND.
    let hash_bytes = match Vec::<u8>::from_hex(payout_payment_hash) {
        Ok(bytes) if bytes.len() == 32 => bytes,
        _ => {
            warn!("Order {order_id}: malformed payout_payment_hash; clearing and re-arming retry");
            crate::db::fail_order_payout(pool, order_id, payout_payment_hash, payout_claimed_at)
                .await?;
            return Ok(());
        }
    };

    match ln_client.lookup_payment_status(&hash_bytes).await {
        Ok(Some(PaymentStatus::Succeeded)) => {
            // Clear the marker only once the order is actually finalized. If
            // finalization fails (or the buyer key is unreadable), keep the
            // marker so a later tick retries it instead of stranding a paid
            // order in settled-hold-invoice.
            let finalized = match Order::by_id(pool, order_id).await {
                Ok(Some(mut order)) => {
                    let my_keys = ctx.keys().clone();
                    match order.get_buyer_pubkey() {
                        Ok(buyer_pubkey) => {
                            payment_success(ctx, &mut order, buyer_pubkey, &my_keys, None)
                                .await
                                .unwrap_or(false)
                        }
                        Err(_) => false,
                    }
                }
                _ => false,
            };
            if finalized {
                crate::db::clear_order_payout(
                    pool,
                    order_id,
                    payout_payment_hash,
                    payout_claimed_at,
                )
                .await?;
            }
        }
        Ok(Some(PaymentStatus::Failed)) | Ok(Some(PaymentStatus::Unknown)) | Ok(None) => {
            // Snapshot the order *before* re-arming so the bookkeeping sees the
            // pre-failure state: `fail_order_payout` sets failed_payment = true,
            // and `count_failed_payment` treats an already-failed order as a
            // subsequent failure (no first-failure notice / attempt bump). This
            // mirrors the in-process watcher, which passes its pre-failure copy.
            let pre = Order::by_id(pool, order_id).await.ok().flatten();
            // Re-arm retry, and — only if we still owned this claim — run the
            // same failure bookkeeping the in-process watcher does, so a payout
            // that resolves only through reconciliation (watcher lost across a
            // restart) still advances payment_attempts and notifies the buyer.
            if crate::db::fail_order_payout(pool, order_id, payout_payment_hash, payout_claimed_at)
                .await?
            {
                if let Some(order) = pre {
                    check_failure_retries_or_log(ctx, &order, None).await;
                }
            }
        }
        Ok(Some(PaymentStatus::InFlight)) => {
            // Still pending — do not re-dispatch; a later tick will reconcile.
        }
        Err(e) => {
            warn!("Order {order_id}: payout reconciliation lookup failed: {e}");
        }
    }

    Ok(())
}

/// Check if order is range type
/// Add parent range id and update max amount
/// publish a new replaceable kind nostr event with the status updated
/// and update on local database the status and new event id
pub async fn get_child_order(
    ctx: &AppContext,
    order: Order,
    my_keys: &Keys,
) -> Result<(Option<Order>, Option<Event>), MostroError> {
    let (Some(max_amount), Some(min_amount)) = (order.max_amount, order.min_amount) else {
        return Ok((None, None));
    };

    if let Some(new_max) = max_amount.checked_sub(order.fiat_amount) {
        let mut new_order = create_base_order(&order)?;

        match new_max.cmp(&min_amount) {
            Ordering::Equal => {
                let (order, event) = order_for_equal(ctx, new_max, &mut new_order, my_keys).await?;
                return Ok((Some(order), Some(event)));
            }
            Ordering::Greater => {
                let (order, event) =
                    order_for_greater(ctx, new_max, &mut new_order, my_keys).await?;
                return Ok((Some(order), Some(event)));
            }
            Ordering::Less => {
                return Ok((None, None));
            }
        }
    }

    Ok((None, None))
}

fn create_base_order(order: &Order) -> Result<Order, MostroError> {
    let mut new_order = order.clone();
    new_order.id = uuid::Uuid::new_v4();
    new_order.status = Status::Pending.to_string();
    new_order.amount = 0;
    new_order.hash = None;
    new_order.preimage = None;
    new_order.buyer_invoice = None;
    new_order.taken_at = 0;
    new_order.invoice_held_at = 0;
    new_order.range_parent_id = Some(order.id);
    // The next-trade rotation is consumed by the time a child is spawned —
    // it is how the child got its counterparty. Never carry it forward.
    new_order.next_trade_index = None;
    new_order.next_trade_pubkey = None;

    match new_order.get_order_kind().map_err(MostroInternalErr)? {
        mostro_core::order::Kind::Sell => {
            new_order.buyer_pubkey = None;
            new_order.master_buyer_pubkey = None;
            new_order.trade_index_buyer = None;
        }
        mostro_core::order::Kind::Buy => {
            new_order.seller_pubkey = None;
            new_order.master_seller_pubkey = None;
            new_order.trade_index_seller = None;
        }
    }

    Ok(new_order)
}

async fn create_order_event(
    ctx: &AppContext,
    new_order: &mut Order,
    my_keys: &Keys,
) -> Result<Event, MostroError> {
    let pool = ctx.pool();

    // Extract user for rating tag
    let identity_pubkey = match new_order.is_sell_order() {
        Ok(_) => new_order
            .get_master_seller_pubkey()
            .map_err(MostroInternalErr)?,
        Err(_) => new_order
            .get_master_buyer_pubkey()
            .map_err(MostroInternalErr)?,
    };

    // If user has sent the order with his identity key means that he wants to be rate so we can just
    // check if we have identity key in db - if present we have to send reputation tags otherwise no.
    let mostro_pubkey = my_keys.public_key().to_hex();
    let tags = match crate::db::is_user_present(pool, identity_pubkey.to_string()).await {
        Ok(user) => order_to_tags(
            new_order,
            Some((user.total_rating, user.total_reviews, user.created_at)),
            Some(&mostro_pubkey),
        )?,
        Err(_) => order_to_tags(new_order, Some((0.0, 0, 0)), Some(&mostro_pubkey))?,
    };

    // Prepare new child order event for sending (kind 38383 for orders).
    // Stamp it through the monotonic registry so a same-second follow-up
    // revision of the child order cannot tie on `created_at`.
    let event = if let Some(tags) = tags {
        let created_at = monotonic_order_event_timestamp(new_order.id, Timestamp::now());
        new_order_event_with_created_at(my_keys, "", new_order.id.to_string(), tags, created_at)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?
    } else {
        return Err(MostroInternalErr(ServiceError::UnexpectedError(
            "Error creating order event".to_string(),
        )));
    };

    new_order.event_id = event.id.to_string();
    Ok(event)
}

async fn order_for_equal(
    ctx: &AppContext,
    new_max: i64,
    new_order: &mut Order,
    my_keys: &Keys,
) -> Result<(Order, Event), MostroError> {
    new_order.fiat_amount = new_max;
    new_order.max_amount = None;
    new_order.min_amount = None;
    let event = create_order_event(ctx, new_order, my_keys).await?;

    Ok((new_order.clone(), event))
}

async fn order_for_greater(
    ctx: &AppContext,
    new_max: i64,
    new_order: &mut Order,
    my_keys: &Keys,
) -> Result<(Order, Event), MostroError> {
    new_order.max_amount = Some(new_max);
    new_order.fiat_amount = 0;
    let event = create_order_event(ctx, new_order, my_keys).await?;

    Ok((new_order.clone(), event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::app::context::AppContext;
    use crate::config::{MESSAGE_QUEUES, MOSTRO_CONFIG};
    use async_trait::async_trait;
    use nostr_sdk::prelude::{Keys, Timestamp};
    use sqlx::SqlitePool;
    use std::sync::Arc;

    /// The `MOSTRO_CONFIG` OnceLock is process-global: set it to the shared
    /// `test_settings()` defaults (idempotent across concurrent tests).
    fn init_global_config() {
        let _ = MOSTRO_CONFIG.set(test_settings());
    }

    async fn create_test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    fn build_ctx(pool: &SqlitePool) -> AppContext {
        TestContextBuilder::new()
            .with_pool(Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build()
    }

    /// Build an `UnwrappedMessage` whose trade key (rumor author / `sender`)
    /// is `pubkey`, mirroring the fixture used by the cancel handler tests.
    fn create_unwrapped_message_with_pubkey(pubkey: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::Order(MessageKind::new(
                Some(uuid::Uuid::new_v4()),
                Some(1),
                None,
                Action::Release,
                None,
            )),
            signature: None,
            sender: pubkey,
            identity: Keys::generate().public_key(),
            created_at: Timestamp::now(),
        }
    }

    /// Sell order in `FiatSent` with a preimage so the hold invoice can be
    /// settled. Master keys equal the trade keys (normal mode, deterministic).
    fn fiat_sent_sell_order(seller: PublicKey, buyer: PublicKey) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::FiatSent.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: seller.to_string(),
            seller_pubkey: Some(seller.to_string()),
            master_seller_pubkey: Some(seller.to_string()),
            buyer_pubkey: Some(buyer.to_string()),
            master_buyer_pubkey: Some(buyer.to_string()),
            preimage: Some("aa".to_string()),
            amount: 21_000,
            fee: 21,
            fiat_amount: 40,
            ..Default::default()
        }
    }

    fn release_message(order_id: uuid::Uuid, payload: Option<Payload>) -> Message {
        Message::new_order(Some(order_id), Some(1), None, Action::Release, payload)
    }

    /// Actions queued on the process-global order queue for a given order id.
    /// The queue is shared across concurrently running tests, so assertions
    /// must always filter by our own order id.
    async fn queued_actions_for(order_id: uuid::Uuid) -> Vec<Action> {
        MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(msg, _)| msg.get_inner_message_kind().id == Some(order_id))
            .map(|(msg, _)| msg.get_inner_message_kind().action.clone())
            .collect()
    }

    struct StubEscrow;

    #[async_trait]
    impl EscrowBackend for StubEscrow {
        async fn create_hold_invoice(
            &mut self,
            _description: &str,
            _amount: i64,
        ) -> Result<(String, Vec<u8>, Vec<u8>), MostroError> {
            Ok(("lnbc1".to_string(), vec![0u8; 32], vec![1u8; 32]))
        }

        async fn settle_hold_invoice(&mut self, _preimage: &str) -> Result<(), MostroError> {
            Ok(())
        }

        async fn cancel_hold_invoice(&mut self, _hash: &str) -> Result<(), MostroError> {
            Ok(())
        }
    }

    struct FailingSettleEscrow;

    #[async_trait]
    impl EscrowBackend for FailingSettleEscrow {
        async fn create_hold_invoice(
            &mut self,
            _description: &str,
            _amount: i64,
        ) -> Result<(String, Vec<u8>, Vec<u8>), MostroError> {
            Ok(("lnbc1".to_string(), vec![0u8; 32], vec![1u8; 32]))
        }

        async fn settle_hold_invoice(&mut self, _preimage: &str) -> Result<(), MostroError> {
            Err(MostroInternalErr(ServiceError::HoldInvoiceError(
                "stub settle failure".to_string(),
            )))
        }

        async fn cancel_hold_invoice(&mut self, _hash: &str) -> Result<(), MostroError> {
            Ok(())
        }
    }

    // ---------------------------------------------------------------
    // check_failure_retries
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn check_failure_retries_notifies_buyer_on_first_failure() {
        // Arrange
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.payment_attempts = 0;
        order.failed_payment = false;
        let order = order.create(&pool).await.unwrap();

        // Act
        let result = check_failure_retries(&ctx, &order, None).await;

        // Assert
        let updated = result.unwrap();
        assert!(updated.failed_payment);
        assert_eq!(updated.payment_attempts, 1);
        let db_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert!(db_order.failed_payment);
        assert_eq!(db_order.payment_attempts, 1);
        assert!(queued_actions_for(order.id)
            .await
            .contains(&Action::PaymentFailed));
    }

    #[tokio::test]
    async fn check_failure_retries_sends_add_invoice_when_retries_exhausted() {
        // Arrange: custom per-ctx settings with a 3-attempt budget.
        let pool = create_test_pool().await;
        let mut settings = test_settings();
        settings.lightning.payment_attempts = 3;
        let ctx = TestContextBuilder::new()
            .with_pool(Arc::new(pool.clone()))
            .with_settings(settings)
            .build();
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.payment_attempts = 3;
        order.failed_payment = true;
        order.amount = 5_000;
        order.fee = 100;
        let order = order.create(&pool).await.unwrap();

        // Act
        let result = check_failure_retries(&ctx, &order, None).await;

        // Assert
        let updated = result.unwrap();
        assert_eq!(updated.payment_attempts, 3);
        assert!(queued_actions_for(order.id)
            .await
            .contains(&Action::AddInvoice));
    }

    #[tokio::test]
    async fn check_failure_retries_rejects_non_positive_amount() {
        // Arrange: amount minus fee is zero on the retry-exhausted branch.
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.payment_attempts = 1;
        order.failed_payment = true;
        order.amount = 100;
        order.fee = 100;

        // Act
        let result = check_failure_retries(&ctx, &order, None).await;

        // Assert
        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidAmount))
        ));
    }

    #[tokio::test]
    async fn check_failure_retries_rejects_invalid_order_kind() {
        // Arrange
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.payment_attempts = 1;
        order.failed_payment = true;
        order.kind = "bogus-kind".to_string();

        // Act
        let result = check_failure_retries(&ctx, &order, None).await;

        // Assert
        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidOrderKind))
        ));
    }

    #[tokio::test]
    async fn check_failure_retries_rejects_invalid_order_status() {
        // Arrange
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.payment_attempts = 1;
        order.failed_payment = true;
        order.status = "bogus-status".to_string();

        // Act
        let result = check_failure_retries(&ctx, &order, None).await;

        // Assert
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidOrderStatus))
        ));
    }

    // ---------------------------------------------------------------
    // release_action
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn release_action_rejects_non_seller_sender() {
        // Arrange
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = fiat_sent_sell_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        let intruder = Keys::generate().public_key();
        let event = create_unwrapped_message_with_pubkey(intruder);
        let msg = release_message(order.id, None);
        let my_keys = Keys::generate();
        let mut escrow = StubEscrow;

        // Act
        let result = release_action(&ctx, msg, &event, &my_keys, &mut escrow).await;

        // Assert
        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPeer))
        ));
    }

    #[tokio::test]
    async fn release_action_rejects_order_with_wrong_status() {
        // Arrange: Active is neither FiatSent nor Dispute.
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.status = Status::Active.to_string();
        let order = order.create(&pool).await.unwrap();
        let event = create_unwrapped_message_with_pubkey(seller);
        let msg = release_message(order.id, None);
        let my_keys = Keys::generate();
        let mut escrow = StubEscrow;

        // Act
        let result = release_action(&ctx, msg, &event, &my_keys, &mut escrow).await;

        // Assert
        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::NotAllowedByStatus))
        ));
    }

    #[tokio::test]
    async fn release_action_propagates_escrow_settle_failure() {
        // Arrange
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = fiat_sent_sell_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        let event = create_unwrapped_message_with_pubkey(seller);
        let msg = release_message(order.id, None);
        let my_keys = Keys::generate();
        let mut escrow = FailingSettleEscrow;

        // Act
        let result = release_action(&ctx, msg, &event, &my_keys, &mut escrow).await;

        // Assert: escrow error propagates and the order does not move.
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::HoldInvoiceError(_)))
        ));
        let db_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(db_order.status, Status::FiatSent.to_string());
    }

    #[tokio::test]
    async fn release_action_settles_and_notifies_on_happy_path() {
        // Arrange: non-range order, no buyer invoice (do_payment fails fast).
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = fiat_sent_sell_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        let event = create_unwrapped_message_with_pubkey(seller);
        let msg = release_message(order.id, None);
        let my_keys = Keys::generate();
        let mut escrow = StubEscrow;

        // Act
        let result = release_action(&ctx, msg, &event, &my_keys, &mut escrow).await;

        // Assert
        assert!(result.is_ok());
        let db_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(db_order.status, Status::SettledHoldInvoice.to_string());
        let actions = queued_actions_for(order.id).await;
        assert!(actions.contains(&Action::Released));
        assert!(actions.contains(&Action::HoldInvoicePaymentSettled));
        assert!(actions.contains(&Action::Rate));
    }

    #[tokio::test]
    async fn release_action_closes_open_dispute() {
        // Arrange: disputed order with an open dispute row.
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.status = Status::Dispute.to_string();
        order.seller_dispute = true;
        let order = order.create(&pool).await.unwrap();
        Dispute::new(order.id, Status::Dispute.to_string())
            .create(&pool)
            .await
            .unwrap();
        let event = create_unwrapped_message_with_pubkey(seller);
        let msg = release_message(order.id, None);
        let my_keys = Keys::generate();
        let mut escrow = StubEscrow;

        // Act
        let result = release_action(&ctx, msg, &event, &my_keys, &mut escrow).await;

        // Assert: order settled and dispute auto-closed as Settled.
        assert!(result.is_ok());
        let db_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(db_order.status, Status::SettledHoldInvoice.to_string());
        let dispute = crate::db::find_dispute_by_order_id(&pool, order.id)
            .await
            .unwrap();
        assert_eq!(dispute.status, DisputeStatus::Settled.to_string());
    }

    #[tokio::test]
    async fn release_action_creates_child_order_for_range_order() {
        // Arrange: sell range order with remaining budget and a next-trade key.
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let next_seller = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.max_amount = Some(100);
        order.min_amount = Some(10);
        order.fiat_amount = 40;
        order.next_trade_pubkey = Some(Keys::generate().public_key().to_string());
        order.next_trade_index = Some(9);
        let order = order.create(&pool).await.unwrap();
        let event = create_unwrapped_message_with_pubkey(seller);
        let msg = release_message(
            order.id,
            Some(Payload::NextTrade(next_seller.to_string(), 2)),
        );
        let my_keys = Keys::generate();
        let mut escrow = StubEscrow;

        // Act
        let result = release_action(&ctx, msg, &event, &my_keys, &mut escrow).await;

        // Assert: a pending child order carries the remaining range.
        assert!(result.is_ok());
        let child: Order =
            sqlx::query_as::<_, Order>("SELECT * FROM orders WHERE range_parent_id = ?")
                .bind(order.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(child.status, Status::Pending.to_string());
        assert_eq!(child.max_amount, Some(60));
        assert_eq!(child.fiat_amount, 0);
        assert_eq!(child.seller_pubkey, Some(next_seller.to_string()));
        assert_eq!(child.trade_index_seller, Some(2));
        assert_eq!(child.next_trade_pubkey, None);
        assert_eq!(child.next_trade_index, None);
        assert!(queued_actions_for(child.id)
            .await
            .contains(&Action::NewOrder));
    }

    #[tokio::test]
    async fn release_action_still_succeeds_when_child_order_creation_fails() {
        // Arrange: range order whose child event cannot be built (no master
        // seller pubkey), driving the `Err` arm of the child-order match.
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.master_seller_pubkey = None;
        order.max_amount = Some(100);
        order.min_amount = Some(10);
        let order = order.create(&pool).await.unwrap();
        let event = create_unwrapped_message_with_pubkey(seller);
        let msg = release_message(order.id, None);
        let my_keys = Keys::generate();
        let mut escrow = StubEscrow;

        // Act
        let result = release_action(&ctx, msg, &event, &my_keys, &mut escrow).await;

        // Assert: release completes, no child order was persisted.
        assert!(result.is_ok());
        let children = sqlx::query("SELECT id FROM orders WHERE range_parent_id = ?")
            .bind(order.id)
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(children.is_empty());
    }

    #[tokio::test]
    async fn release_action_pays_buyer_when_release_omits_next_trade_on_sell_range() {
        // Arrange: sell range with a valid remainder, so get_child_order
        // returns Ok(Some, Some), but the Release carries no NextTrade —
        // handle_child_order fails after the hold invoice was settled.
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.max_amount = Some(100);
        order.min_amount = Some(10);
        order.fiat_amount = 40;
        let order = order.create(&pool).await.unwrap();
        let event = create_unwrapped_message_with_pubkey(seller);
        let msg = release_message(order.id, None);
        let my_keys = Keys::generate();
        let mut escrow = StubEscrow;

        // Act
        let result = release_action(&ctx, msg, &event, &my_keys, &mut escrow).await;

        // Assert: release completes instead of aborting, the remainder is
        // skipped (no child row) and the buyer-payout flow still runs.
        assert!(result.is_ok());
        let db_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(db_order.status, Status::SettledHoldInvoice.to_string());
        let children = sqlx::query("SELECT id FROM orders WHERE range_parent_id = ?")
            .bind(order.id)
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(children.is_empty());
        let actions = queued_actions_for(order.id).await;
        assert!(actions.contains(&Action::Released));
        assert!(actions.contains(&Action::HoldInvoicePaymentSettled));
        assert!(actions.contains(&Action::Rate));
    }

    // ---------------------------------------------------------------
    // handle_buy_child_order / handle_sell_child_order
    // ---------------------------------------------------------------

    #[test]
    fn handle_buy_child_order_requires_next_trade_pubkey() {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = fiat_sent_sell_order(seller, buyer);
        let mut child = order.clone();

        let result = handle_buy_child_order(&mut child, &order, None);

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::UnexpectedError(_)))
        ));
    }

    #[test]
    fn handle_buy_child_order_uses_identity_key_in_normal_mode() {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let next_buyer = Keys::generate().public_key();
        let idkey = Keys::generate().public_key().to_string();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.next_trade_pubkey = Some(next_buyer.to_string());
        order.next_trade_index = Some(5);
        let mut child = order.clone();

        let (notify_pubkey, trade_index) =
            handle_buy_child_order(&mut child, &order, Some(idkey.clone())).unwrap();

        assert_eq!(notify_pubkey, Some(next_buyer.to_string()));
        assert_eq!(trade_index, Some(5));
        assert_eq!(child.master_buyer_pubkey, Some(idkey));
        assert_eq!(child.creator_pubkey, next_buyer.to_string());
    }

    #[test]
    fn handle_buy_child_order_uses_trade_key_in_full_privacy_mode() {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let next_buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.next_trade_pubkey = Some(next_buyer.to_string());
        order.next_trade_index = Some(7);
        let mut child = order.clone();

        handle_buy_child_order(&mut child, &order, None).unwrap();

        assert_eq!(child.master_buyer_pubkey, Some(next_buyer.to_string()));
    }

    #[test]
    fn handle_sell_child_order_requires_next_trade() {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut child = fiat_sent_sell_order(seller, buyer);

        let result = handle_sell_child_order(&mut child, None, None);

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::UnexpectedError(_)))
        ));
    }

    #[test]
    fn handle_sell_child_order_rejects_invalid_pubkey() {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut child = fiat_sent_sell_order(seller, buyer);

        let result =
            handle_sell_child_order(&mut child, Some(("not-a-pubkey".to_string(), 1)), None);

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
    }

    #[test]
    fn handle_sell_child_order_sets_seller_fields_for_both_privacy_modes() {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let next_seller = Keys::generate().public_key();
        let idkey = Keys::generate().public_key().to_string();

        // Normal mode: identity key becomes the master seller pubkey.
        let mut child = fiat_sent_sell_order(seller, buyer);
        let (notify_pubkey, trade_index) = handle_sell_child_order(
            &mut child,
            Some((next_seller.to_string(), 3)),
            Some(idkey.clone()),
        )
        .unwrap();
        assert_eq!(notify_pubkey, Some(next_seller.to_string()));
        assert_eq!(trade_index, Some(3));
        assert_eq!(child.master_seller_pubkey, Some(idkey));
        assert_eq!(child.creator_pubkey, next_seller.to_string());

        // Full privacy mode: the next trade key doubles as master pubkey.
        let mut child = fiat_sent_sell_order(seller, buyer);
        handle_sell_child_order(&mut child, Some((next_seller.to_string(), 3)), None).unwrap();
        assert_eq!(child.master_seller_pubkey, Some(next_seller.to_string()));
    }

    // ---------------------------------------------------------------
    // handle_child_order
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn handle_child_order_creates_buy_child_when_creator_is_buyer() {
        // Arrange
        let pool = create_test_pool().await;
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let next_buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.kind = mostro_core::order::Kind::Buy.to_string();
        order.creator_pubkey = buyer.to_string();
        order.next_trade_pubkey = Some(next_buyer.to_string());
        order.next_trade_index = Some(4);
        let child = create_base_order(&order).unwrap();

        // Act
        let result = handle_child_order(child.clone(), &order, None, &pool, None).await;

        // Assert: child persisted and next buyer notified.
        assert!(result.is_ok());
        let db_child = Order::by_id(&pool, child.id).await.unwrap().unwrap();
        assert_eq!(db_child.buyer_pubkey, Some(next_buyer.to_string()));
        assert_eq!(db_child.trade_index_buyer, Some(4));
        assert_eq!(db_child.next_trade_pubkey, None);
        assert_eq!(db_child.next_trade_index, None);
        assert!(queued_actions_for(child.id)
            .await
            .contains(&Action::NewOrder));
    }

    #[tokio::test]
    async fn handle_child_order_creates_sell_child_when_creator_is_seller() {
        // Arrange
        let pool = create_test_pool().await;
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let next_seller = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        // Seed the rotation on the parent. `create_base_order` is the only
        // path that builds a child, so this is the shape production sees.
        order.next_trade_pubkey = Some(Keys::generate().public_key().to_string());
        order.next_trade_index = Some(9);
        let child = create_base_order(&order).unwrap();

        // Act
        let result = handle_child_order(
            child.clone(),
            &order,
            Some((next_seller.to_string(), 6)),
            &pool,
            None,
        )
        .await;

        // Assert
        assert!(result.is_ok());
        let db_child = Order::by_id(&pool, child.id).await.unwrap().unwrap();
        assert_eq!(db_child.seller_pubkey, Some(next_seller.to_string()));
        assert_eq!(db_child.trade_index_seller, Some(6));
        assert_eq!(db_child.next_trade_pubkey, None);
        assert_eq!(db_child.next_trade_index, None);
    }

    #[tokio::test]
    async fn handle_child_order_rejects_invalid_type_or_creator() {
        // Arrange: sell order whose creator is the buyer — neither branch fits.
        let pool = create_test_pool().await;
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.creator_pubkey = buyer.to_string();
        let child = order.clone();

        // Act
        let result = handle_child_order(child, &order, None, &pool, None).await;

        // Assert
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::UnexpectedError(_)))
        ));
    }

    #[tokio::test]
    async fn handle_child_order_fails_when_next_trade_is_missing() {
        // Arrange: sell-creator branch without a next trade key.
        let pool = create_test_pool().await;
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = fiat_sent_sell_order(seller, buyer);
        let child = order.clone();

        // Act
        let result = handle_child_order(child, &order, None, &pool, None).await;

        // Assert
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::UnexpectedError(_)))
        ));
    }

    // ---------------------------------------------------------------
    // create_base_order / get_child_order
    // ---------------------------------------------------------------

    #[test]
    fn create_base_order_resets_trade_state_for_sell_orders() {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.next_trade_pubkey = Some(Keys::generate().public_key().to_string());
        order.next_trade_index = Some(9);

        let base = create_base_order(&order).unwrap();

        assert_ne!(base.id, order.id);
        assert_eq!(base.status, Status::Pending.to_string());
        assert_eq!(base.amount, 0);
        assert_eq!(base.preimage, None);
        assert_eq!(base.buyer_invoice, None);
        assert_eq!(base.range_parent_id, Some(order.id));
        // Sell orders clear the buyer side.
        assert_eq!(base.buyer_pubkey, None);
        assert_eq!(base.master_buyer_pubkey, None);
        assert_eq!(base.trade_index_buyer, None);
        // ... and keep the seller side.
        assert_eq!(base.seller_pubkey, order.seller_pubkey);
        // ... and drop the consumed next-trade rotation.
        assert_eq!(base.next_trade_pubkey, None);
        assert_eq!(base.next_trade_index, None);
    }

    #[test]
    fn create_base_order_clears_seller_side_for_buy_orders() {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.kind = mostro_core::order::Kind::Buy.to_string();

        let base = create_base_order(&order).unwrap();

        assert_eq!(base.seller_pubkey, None);
        assert_eq!(base.master_seller_pubkey, None);
        assert_eq!(base.trade_index_seller, None);
        assert_eq!(base.buyer_pubkey, order.buyer_pubkey);
    }

    #[test]
    fn create_base_order_rejects_invalid_kind() {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.kind = "bogus-kind".to_string();

        assert!(create_base_order(&order).is_err());
    }

    #[tokio::test]
    async fn get_child_order_returns_none_for_non_range_order() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = fiat_sent_sell_order(seller, buyer);
        let my_keys = Keys::generate();

        let (child, event) = get_child_order(&ctx, order, &my_keys).await.unwrap();

        assert!(child.is_none());
        assert!(event.is_none());
    }

    #[tokio::test]
    async fn get_child_order_consumes_range_when_remainder_equals_min() {
        // Arrange: user present in db → rating branch of create_order_event.
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.max_amount = Some(100);
        order.min_amount = Some(50);
        order.fiat_amount = 50;
        crate::db::add_new_user(
            &pool,
            mostro_core::user::User::new(seller.to_string(), 0, 0, 0, 0, 0),
        )
        .await
        .unwrap();
        let my_keys = Keys::generate();

        // Act
        let (child, event) = get_child_order(&ctx, order, &my_keys).await.unwrap();

        // Assert: the child becomes a fixed-amount order at the minimum.
        let child = child.unwrap();
        let event = event.unwrap();
        assert_eq!(child.fiat_amount, 50);
        assert_eq!(child.max_amount, None);
        assert_eq!(child.min_amount, None);
        assert_eq!(child.event_id, event.id.to_string());
    }

    #[tokio::test]
    async fn get_child_order_keeps_range_when_remainder_exceeds_min() {
        // Arrange: user absent from db → fallback rating branch.
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.max_amount = Some(100);
        order.min_amount = Some(10);
        order.fiat_amount = 40;
        let my_keys = Keys::generate();

        // Act
        let (child, event) = get_child_order(&ctx, order, &my_keys).await.unwrap();

        // Assert: the child keeps a shrunk range.
        let child = child.unwrap();
        assert!(event.is_some());
        assert_eq!(child.max_amount, Some(60));
        assert_eq!(child.min_amount, Some(10));
        assert_eq!(child.fiat_amount, 0);
    }

    #[tokio::test]
    async fn get_child_order_returns_none_when_remainder_below_min() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.max_amount = Some(100);
        order.min_amount = Some(80);
        order.fiat_amount = 50;
        let my_keys = Keys::generate();

        let (child, event) = get_child_order(&ctx, order, &my_keys).await.unwrap();

        assert!(child.is_none());
        assert!(event.is_none());
    }

    #[tokio::test]
    async fn get_child_order_returns_none_on_subtraction_overflow() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.max_amount = Some(i64::MIN);
        order.min_amount = Some(0);
        order.fiat_amount = 1;
        let my_keys = Keys::generate();

        let (child, event) = get_child_order(&ctx, order, &my_keys).await.unwrap();

        assert!(child.is_none());
        assert!(event.is_none());
    }

    // ---------------------------------------------------------------
    // do_payment / payment_success
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn do_payment_fails_without_buyer_invoice() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = fiat_sent_sell_order(seller, buyer);

        let result = do_payment(&ctx, order, None).await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError))
        ));
    }

    #[tokio::test]
    async fn do_payment_fails_when_amount_is_consumed_by_fee() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.buyer_invoice = Some("lnbc1notchecked".to_string());
        order.amount = 100;
        order.fee = 100;

        let result = do_payment(&ctx, order, None).await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvoiceInvalidError))
        ));
    }

    #[tokio::test]
    async fn do_payment_fails_fast_when_lnd_is_unreachable() {
        // Arrange: with the global config set to test defaults, the LND cert
        // path is invalid, so LndConnector::new() returns an error without
        // any network access.
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.buyer_invoice = Some("lnbc1notchecked".to_string());

        // Act
        let result = do_payment(&ctx, order, None).await;

        // Assert
        assert!(result.is_err());
    }

    /// An expired regtest invoice: rejected by `validate_payout_invoice` on
    /// every chain, either for the currency or for having expired, so the
    /// assertion below does not depend on what `LN_STATUS` holds.
    const EXPIRED_INVOICE: &str = "lnbcrt500u1p3lzwdzpp5t9kgwgwd07y2lrwdscdnkqu4scrcgpm5pt9uwx0rxn5rxawlxlvqdqqcqzpgxqyz5vqsp5a6k7syfxeg8jy63rteywwjla5rrg2pvhedx8ajr2ltm4seydhsqq9qyyssq0n2uwlumsx4d0mtjm8tp7jw3y4da6p6z9gyyjac0d9xugf72lhh4snxpugek6n83geafue9ndgrhuhzk98xcecu2t3z56ut35mkammsqscqp0n";

    /// Stand-in LNURL-pay server: the well-known endpoint points at its own
    /// callback, which hands back `EXPIRED_INVOICE`.
    async fn start_lnurl_test_server() -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::get, Json, Router};
        use serde_json::json;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let callback = format!("http://127.0.0.1:{port}/callback");

        let app = Router::new()
            .route(
                "/.well-known/lnurlp/payout",
                get(move || {
                    let callback = callback.clone();
                    async move {
                        Json(json!({
                            "tag": "payRequest",
                            "callback": callback,
                            "minSendable": 1,
                            "maxSendable": 100_000_000_000u64,
                            "metadata": "[[\"text/plain\",\"payout\"]]"
                        }))
                    }
                }),
            )
            .route(
                "/callback",
                get(|| async { Json(json!({ "pr": EXPIRED_INVOICE })) }),
            );

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let lnurl = LnUrl {
            url: format!("http://127.0.0.1:{port}/.well-known/lnurlp/payout"),
        }
        .encode();

        (lnurl, handle)
    }

    /// An encoded LNURL is a payout destination `is_valid_invoice` accepts, so
    /// `do_payment` has to resolve it and validate the invoice it gets back.
    /// Reaching `InvoiceInvalidError` proves both happened: unresolved, the
    /// LNURL string would have travelled on to the LND connector instead.
    #[tokio::test]
    async fn do_payment_resolves_and_validates_an_encoded_lnurl() {
        init_global_config();
        // The mock LNURL server binds on loopback, which the LNURL host policy
        // forbids in production. Take the policy lock before flipping the flag
        // so a concurrent test cannot observe private hosts as allowed.
        let _policy_lock = crate::lnurl::AllowPrivateLnurlHostsGuard::lock_policy().await;
        let _allow_private = crate::lnurl::AllowPrivateLnurlHostsGuard::enable();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let (lnurl, server) = start_lnurl_test_server().await;

        let mut order = fiat_sent_sell_order(seller, buyer);
        order.buyer_invoice = Some(lnurl);

        let result = do_payment(&ctx, order, None).await;

        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::InvoiceInvalidError))
            ),
            "the resolved invoice must be rejected before LND is contacted: {result:?}"
        );

        server.abort();
    }

    /// Produce a real `tokio::time::error::Elapsed` (it has no public
    /// constructor): a zero-duration timeout over a pending future.
    async fn elapsed() -> tokio::time::error::Elapsed {
        timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err()
    }

    fn send_err() -> MostroError {
        MostroInternalErr(ServiceError::LnPaymentError("boom".to_string()))
    }

    #[tokio::test]
    async fn dispatch_stream_end_needs_no_claim_action() {
        assert_eq!(
            classify_dispatch(Ok(Ok(())), None),
            DispatchVerdict::StreamEnded
        );
    }

    #[tokio::test]
    async fn dispatch_timeout_keeps_the_marker() {
        // The central safety invariant of the background dispatch: a
        // timed-out send must NEVER re-arm retry — the HTLC may still
        // settle, and re-arming against it risks a double payout.
        match classify_dispatch(Err(elapsed().await), None) {
            DispatchVerdict::KeepMarker(cause) => assert!(
                cause.contains(&format!(
                    "no terminal state after {}s",
                    PAYOUT_SEND_PAYMENT_TIMEOUT.as_secs()
                )),
                "cause must name the timeout: {cause}"
            ),
            other => panic!("a timed-out send must keep the marker, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_rpc_error_with_inflight_payment_keeps_the_marker() {
        match classify_dispatch(Ok(Err(send_err())), Some(Ok(Some(PaymentStatus::InFlight)))) {
            DispatchVerdict::KeepMarker(cause) => {
                assert!(cause.contains("boom"), "cause must carry the send error")
            }
            other => panic!("an in-flight payment must keep the marker, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_rpc_error_with_settled_payment_keeps_the_marker() {
        assert!(matches!(
            classify_dispatch(
                Ok(Err(send_err())),
                Some(Ok(Some(PaymentStatus::Succeeded)))
            ),
            DispatchVerdict::KeepMarker(_)
        ));
    }

    #[tokio::test]
    async fn dispatch_rpc_error_with_failed_lookup_keeps_the_marker() {
        // An unanswerable lookup cannot rule out an in-flight payment, so
        // the conservative direction is to keep the claim.
        assert!(matches!(
            classify_dispatch(
                Ok(Err(send_err())),
                Some(Err(MostroInternalErr(ServiceError::LnPaymentError(
                    "lookup down".to_string()
                ))))
            ),
            DispatchVerdict::KeepMarker(_)
        ));
    }

    #[tokio::test]
    async fn dispatch_rpc_error_with_failed_payment_rearms() {
        match classify_dispatch(Ok(Err(send_err())), Some(Ok(Some(PaymentStatus::Failed)))) {
            DispatchVerdict::ReArm(cause) => {
                assert!(cause.contains("boom"), "cause must carry the send error")
            }
            other => panic!("a failed payment must re-arm retry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_rpc_error_with_unknown_payment_rearms() {
        assert!(matches!(
            classify_dispatch(Ok(Err(send_err())), Some(Ok(Some(PaymentStatus::Unknown)))),
            DispatchVerdict::ReArm(_)
        ));
    }

    #[tokio::test]
    async fn dispatch_rpc_error_with_no_lnd_record_rearms() {
        assert!(matches!(
            classify_dispatch(Ok(Err(send_err())), Some(Ok(None))),
            DispatchVerdict::ReArm(_)
        ));
    }

    #[tokio::test]
    async fn dispatch_rpc_error_with_unusable_hash_rearms() {
        // No lookup was possible (hash undecodable): an in-flight payment
        // cannot be confirmed, so re-arm rather than strand the payout.
        assert!(matches!(
            classify_dispatch(Ok(Err(send_err())), None),
            DispatchVerdict::ReArm(_)
        ));
    }

    #[tokio::test]
    async fn payment_success_transitions_settled_order_to_success() {
        // Arrange
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.status = Status::SettledHoldInvoice.to_string();
        let mut order = order.create(&pool).await.unwrap();
        let my_keys = Keys::generate();

        // Act
        let result = payment_success(&ctx, &mut order, buyer, &my_keys, None).await;

        // Assert: committed the transition (returns true) and notified the buyer.
        assert!(result.unwrap(), "a committed finalization returns true");
        let db_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(db_order.status, Status::Success.to_string());
        let actions = queued_actions_for(order.id).await;
        assert!(actions.contains(&Action::PurchaseCompleted));
        assert!(actions.contains(&Action::Rate));
    }

    #[tokio::test]
    async fn payment_success_skips_orders_already_processed() {
        // Arrange: DB row is Active, so the guarded UPDATE matches no rows.
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.status = Status::Active.to_string();
        let mut order = order.create(&pool).await.unwrap();
        let my_keys = Keys::generate();

        // Act
        let result = payment_success(&ctx, &mut order, buyer, &my_keys, None).await;

        // Assert: the guarded UPDATE matched no rows (already finalized
        // elsewhere), so the call reports terminal (`true`) — the caller may
        // release the marker — but sends no duplicate notifications, and the
        // status is left untouched.
        assert!(
            result.unwrap(),
            "a no-op CAS (already processed) is terminal and returns true"
        );
        let db_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(db_order.status, Status::Active.to_string());
        let actions = queued_actions_for(order.id).await;
        assert!(!actions.contains(&Action::PurchaseCompleted));
        assert!(!actions.contains(&Action::Rate));
    }

    // --- reconcile_inflight_payout branch coverage (stubbed LND) ---

    /// Configurable LND stub: returns a fixed `PaymentStatus` (or an error) for
    /// every `lookup_payment_status`, so the four reconcile branches can be
    /// exercised without a live node.
    struct StubLnClient {
        status: Option<PaymentStatus>,
        error: bool,
    }

    impl PayoutStatusLookup for StubLnClient {
        fn lookup_payment_status<'a>(
            &'a mut self,
            _payment_hash: &'a [u8],
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<PaymentStatus>, MostroError>>
                    + Send
                    + 'a,
            >,
        > {
            let status = self.status;
            let error = self.error;
            Box::pin(async move {
                if error {
                    Err(MostroInternalErr(ServiceError::LnPaymentError(
                        "stub".to_string(),
                    )))
                } else {
                    Ok(status)
                }
            })
        }
    }

    /// Create a `settled-hold-invoice` order and claim a payout marker on it,
    /// returning `(order_id, payout_hash, claim_token)`.
    async fn settled_order_with_marker(pool: &SqlitePool) -> (uuid::Uuid, String, i64) {
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.status = Status::SettledHoldInvoice.to_string();
        let order = order.create(pool).await.unwrap();
        let hash = "a".repeat(64);
        let token = crate::db::claim_order_payout(pool, order.id, &hash)
            .await
            .unwrap()
            .expect("claim must win on a fresh order");
        (order.id, hash, token)
    }

    async fn marker_of(pool: &SqlitePool, id: uuid::Uuid) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT payout_payment_hash FROM orders WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn reconcile_succeeded_finalizes_and_clears_marker() {
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let (id, hash, token) = settled_order_with_marker(&pool).await;
        let mut ln = StubLnClient {
            status: Some(PaymentStatus::Succeeded),
            error: false,
        };

        reconcile_inflight_payout(&ctx, &mut ln, id, &hash, Some(token))
            .await
            .unwrap();

        let db_order = Order::by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!(db_order.status, Status::Success.to_string());
        assert!(
            marker_of(&pool, id).await.is_none(),
            "marker released after finalize"
        );
    }

    #[tokio::test]
    async fn reconcile_failed_rearms_and_runs_bookkeeping() {
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let (id, hash, token) = settled_order_with_marker(&pool).await;
        let mut ln = StubLnClient {
            status: Some(PaymentStatus::Failed),
            error: false,
        };

        reconcile_inflight_payout(&ctx, &mut ln, id, &hash, Some(token))
            .await
            .unwrap();

        assert!(marker_of(&pool, id).await.is_none(), "marker released");
        let db_order = Order::by_id(&pool, id).await.unwrap().unwrap();
        assert!(db_order.failed_payment, "retry re-armed");
        assert_eq!(
            db_order.payment_attempts, 1,
            "bookkeeping advanced payment_attempts"
        );
        assert!(
            queued_actions_for(id)
                .await
                .contains(&Action::PaymentFailed),
            "buyer notified on first failure"
        );
    }

    #[tokio::test]
    async fn reconcile_inflight_is_a_noop() {
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let (id, hash, token) = settled_order_with_marker(&pool).await;
        let mut ln = StubLnClient {
            status: Some(PaymentStatus::InFlight),
            error: false,
        };

        reconcile_inflight_payout(&ctx, &mut ln, id, &hash, Some(token))
            .await
            .unwrap();

        assert_eq!(
            marker_of(&pool, id).await.as_deref(),
            Some(hash.as_str()),
            "an in-flight payout keeps its marker"
        );
        let db_order = Order::by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!(db_order.status, Status::SettledHoldInvoice.to_string());
        assert!(!db_order.failed_payment, "in-flight must not re-arm retry");
    }

    #[tokio::test]
    async fn reconcile_malformed_hash_rearms_without_lookup() {
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = fiat_sent_sell_order(seller, buyer);
        order.status = Status::SettledHoldInvoice.to_string();
        let order = order.create(&pool).await.unwrap();
        // Force a corrupt (non-32-byte) marker directly.
        sqlx::query(
            "UPDATE orders SET payout_payment_hash = ?, payout_claimed_at = ? WHERE id = ?",
        )
        .bind("abc")
        .bind(1000_i64)
        .bind(order.id)
        .execute(&pool)
        .await
        .unwrap();

        // Stub set to error to prove it is never consulted for a malformed hash.
        let mut ln = StubLnClient {
            status: None,
            error: true,
        };
        reconcile_inflight_payout(&ctx, &mut ln, order.id, "abc", Some(1000))
            .await
            .unwrap();

        assert!(
            marker_of(&pool, order.id).await.is_none(),
            "malformed marker cleared"
        );
        let db_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert!(db_order.failed_payment, "malformed marker re-arms retry");
    }
}
