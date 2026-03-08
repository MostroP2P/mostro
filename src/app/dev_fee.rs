//! Dev fee payment processing.
//!
//! Handles the full lifecycle of development fee payments: resolving LNURL
//! invoices, sending Lightning payments, crash recovery, and idempotency
//! checks to prevent duplicate payments (#620). The scheduler calls
//! [`run_dev_fee_cycle`] once per tick; all state‑machine logic lives here.
//!
//! # Full cycle of dev fee payment
//!
//! Each tick runs four phases in order. Later phases rely on DB state left
//! by earlier ones.
//!
//! ## Phase 1: Stale PENDING cleanup
//!
//! Orders with `dev_fee_payment_hash` like `PENDING-{uuid}-{ts}` older than
//! 5 minutes are reset (hash cleared, `dev_fee_paid = false`). They become
//! eligible again for Phase 4 so a fresh payment can be attempted.
//!
//! ## Phase 2: Verify confirmed orders
//!
//! Orders already marked `dev_fee_paid = 1` with a real (non-PENDING) hash
//! are re-checked against the LN node. On daemon restart the in-memory
//! `confirmed` set is empty, so every paid order is verified once. If LN
//! reports success, the order is added to `confirmed`. Failed/Unknown are
//! not reset to avoid duplicate payments (LNURL gives a new invoice each time).
//!
//! ## Phase 3: Recover partial payments
//!
//! Orders with a real payment hash but `dev_fee_paid = 0` are “partial”:
//! the daemon stored the hash and then crashed before LND confirmed. This
//! phase checks LN status for that hash. If **Succeeded**, the order is
//! updated to paid and added to `confirmed`. If **Failed**, the hash is
//! cleared so the next cycle can retry with a new invoice. InFlight/Unknown
//! are left as-is to avoid duplicate payment risk.
//!
//! ## Phase 4: Process new dev fee payments
//!
//! Orders with no existing payment hash (or empty/PENDING) are processed:
//!
//! 1. **Claim** — Atomic DB update sets `dev_fee_payment_hash` to a unique
//!    `PENDING-{uuid}-{ts}` so only one scheduler cycle owns the order.
//! 2. **Resolve** — LNURL resolution for the dev fee lightning address yields
//!    a payment request and the real payment hash. On failure or timeout,
//!    the PENDING claim is released so the order can be retried later.
//! 3. **Store hash** — The real hash and `dev_fee_paid = true` are written
//!    to the DB *before* sending the payment. If the daemon crashes after
//!    this, Phase 3 will find the hash and verify with LND on the next run.
//! 4. **Send payment** — Lightning payment is sent (50s timeout). Outcome:
//!    - **Success** — DB is updated to reflect payment, order is added to
//!      `confirmed`, and an audit event may be published.
//!    - **Failure** — `dev_fee_paid` is set false and DB updated; hash is
//!      kept so the idempotency check (Phase 3 next cycle) can verify with LND.
//!    - **Timeout** — LN status is checked; if Succeeded, order is updated
//!      and confirmed; if Failed, hash is cleared for retry; InFlight/Unknown
//!      leave the hash in place to avoid duplicates.
//!
//! Key principle: the real payment hash is always stored before sending, and
//! recovery paths (Phase 3 and timeout handling) query LND by that hash
//! instead of resolving a new invoice, preventing double payment.

use crate::config::constants::DEV_FEE_LIGHTNING_ADDRESS;
use crate::db::find_unpaid_dev_fees;
use crate::lightning::invoice::decode_invoice;
use crate::lightning::LndConnector;
use crate::lnurl::resolv_ln_address;
use crate::util::{bytes_to_string, publish_dev_fee_audit_event};

use chrono::Utc;
use mostro_core::error::MostroError;
use mostro_core::error::MostroError::MostroInternalErr;
use mostro_core::error::ServiceError;
use mostro_core::order::Order;
use sqlx::SqlitePool;
use sqlx_crud::Crud;
use std::collections::HashSet;
use tokio::sync::mpsc::channel;
use tracing::{error, info, warn};

// ── Public entry point ──────────────────────────────────────────────────

/// Run one full dev‑fee processing cycle.
///
/// Called by the scheduler every tick. Phases run sequentially so each
/// phase can rely on the DB state left by the previous one.
#[mutants::skip]
pub async fn run_dev_fee_cycle(
    pool: &SqlitePool,
    ln_client: &mut LndConnector,
    confirmed: &mut HashSet<uuid::Uuid>,
) {
    info!("Checking for unpaid development fees");

    cleanup_stale_pending_markers(pool).await;
    verify_confirmed_orders(pool, ln_client, confirmed).await;
    recover_partial_payments(pool, ln_client, confirmed).await;
    process_new_dev_fee_payments(pool, ln_client, confirmed).await;
}

// ── Phase 1: Stale PENDING cleanup ──────────────────────────────────────

/// Reset PENDING markers older than `CLEANUP_TTL_SECS` so those orders
/// become eligible for a fresh payment attempt on the next cycle.
async fn cleanup_stale_pending_markers(pool: &SqlitePool) {
    const CLEANUP_TTL_SECS: u64 = 300; // 5 minutes
    let now_unix = Utc::now().timestamp() as u64;

    let pending_orders = match sqlx::query_as::<_, Order>(
        "SELECT * FROM orders WHERE dev_fee_payment_hash LIKE 'PENDING-%'",
    )
    .fetch_all(pool)
    .await
    {
        Ok(orders) => orders,
        Err(e) => {
            error!("Failed to query stale PENDING orders: {:?}", e);
            return;
        }
    };

    let mut stale_count = 0u32;

    for mut pending_order in pending_orders {
        let order_id = pending_order.id;
        let marker = pending_order
            .dev_fee_payment_hash
            .as_deref()
            .unwrap_or_default();

        let pending_ts = parse_pending_timestamp(marker);

        let is_stale = match pending_ts {
            Some(ts) => now_unix.saturating_sub(ts) >= CLEANUP_TTL_SECS,
            None => {
                warn!(
                    "Order {} has legacy PENDING marker without timestamp, treating as stale",
                    order_id
                );
                true
            }
        };

        if !is_stale {
            continue;
        }

        stale_count += 1;
        let age_display = pending_ts
            .map(|ts| format!("{}s", now_unix.saturating_sub(ts)))
            .unwrap_or_else(|| "unknown (legacy)".to_string());

        warn!(
            "Resetting stale PENDING order {} (age: {})",
            order_id, age_display
        );

        pending_order.dev_fee_paid = false;
        pending_order.dev_fee_payment_hash = None;

        match pending_order.update(pool).await {
            Ok(_) => {
                info!(
                    "Reset stale PENDING for order {}, will retry payment",
                    order_id
                );
            }
            Err(e) => {
                error!(
                    "Failed to reset stale PENDING for order {}: {:?}",
                    order_id, e
                );
            }
        }
    }

    if stale_count > 0 {
        warn!(
            "Reset {} stale PENDING dev fee orders (TTL: {}s)",
            stale_count, CLEANUP_TTL_SECS
        );
    }
}

// ── Phase 2: Verify already‑paid orders against LN node ────────────────

/// For orders marked `dev_fee_paid=1` with a real hash, confirm the
/// payment actually succeeded on the LN node. On daemon restart the
/// `confirmed` set is empty so every paid order gets re‑checked once.
async fn verify_confirmed_orders(
    pool: &SqlitePool,
    ln_client: &mut LndConnector,
    confirmed: &mut HashSet<uuid::Uuid>,
) {
    let real_hash_orders = match sqlx::query_as::<_, Order>(
        "SELECT * FROM orders
         WHERE dev_fee_paid = 1
           AND dev_fee_payment_hash IS NOT NULL
           AND dev_fee_payment_hash NOT LIKE 'PENDING-%'
           AND (status = 'settled-hold-invoice' OR status = 'success')",
    )
    .fetch_all(pool)
    .await
    {
        Ok(orders) => orders,
        Err(e) => {
            error!("Failed to query confirmed dev fee orders: {:?}", e);
            return;
        }
    };

    for real_hash_order in real_hash_orders {
        let order_id = real_hash_order.id;

        if confirmed.contains(&order_id) {
            continue;
        }

        match check_dev_fee_payment_status(&real_hash_order, ln_client).await {
            DevFeePaymentState::Succeeded => {
                confirmed.insert(order_id);
            }
            DevFeePaymentState::Failed => {
                // Do NOT reset orders with real payment hashes. LND may report
                // "Failed" for payments that haven't been fully indexed yet.
                // Resetting prematurely causes duplicate payments because LNURL
                // resolution generates a NEW invoice every time (see #620).
                warn!(
                    "Dev fee payment reported as Failed for order {} (hash: {:?}), \
                     NOT resetting to avoid duplicate payment risk. \
                     Manual review may be needed.",
                    order_id, real_hash_order.dev_fee_payment_hash
                );
            }
            DevFeePaymentState::InFlight | DevFeePaymentState::Unknown => {}
        }
    }
}

// ── Phase 3: Recover partial payments (hash stored, not yet confirmed) ──

/// Orders that have a real payment hash but `dev_fee_paid=0` represent
/// a crash between "store hash" and "receive LND confirmation". This is
/// the PRIMARY defense against duplicate payments (#620): reuse the
/// existing hash instead of resolving a new LNURL invoice.
async fn recover_partial_payments(
    pool: &SqlitePool,
    ln_client: &mut LndConnector,
    confirmed: &mut HashSet<uuid::Uuid>,
) {
    let hash_orders = match sqlx::query_as::<_, Order>(
        "SELECT * FROM orders
         WHERE (status = 'settled-hold-invoice' OR status = 'success')
           AND dev_fee > 0
           AND dev_fee_paid = 0
           AND dev_fee_payment_hash IS NOT NULL
           AND dev_fee_payment_hash != ''
           AND dev_fee_payment_hash NOT LIKE 'PENDING-%'",
    )
    .fetch_all(pool)
    .await
    {
        Ok(orders) => orders,
        Err(e) => {
            error!("Failed to query partial-payment orders: {:?}", e);
            return;
        }
    };

    for mut hash_order in hash_orders {
        let order_id = hash_order.id;
        let existing_hash = hash_order.dev_fee_payment_hash.clone().unwrap_or_default();

        info!(
            "Order {} has existing payment hash '{}' but dev_fee_paid=0, checking LN status",
            order_id, existing_hash
        );

        match check_dev_fee_payment_status(&hash_order, ln_client).await {
            DevFeePaymentState::Succeeded => {
                info!(
                    "Order {} payment already succeeded (hash {}), marking as paid",
                    order_id, existing_hash
                );
                hash_order.dev_fee_paid = true;
                match hash_order.update(pool).await {
                    Ok(updated) => {
                        confirmed.insert(order_id);
                        if let Err(e) = publish_dev_fee_audit_event(&updated, &existing_hash).await
                        {
                            warn!(
                                "Failed to publish audit event for recovered order {}: {:?}",
                                order_id, e
                            );
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to mark order {} as paid after confirming payment: {:?}",
                            order_id, e
                        );
                    }
                }
            }
            DevFeePaymentState::Failed => {
                info!(
                    "Order {} existing payment failed (hash {}), clearing for retry",
                    order_id, existing_hash
                );
                hash_order.dev_fee_payment_hash = None;
                if let Err(e) = hash_order.update(pool).await {
                    error!(
                        "Failed to clear failed payment hash for order {}: {:?}",
                        order_id, e
                    );
                }
            }
            DevFeePaymentState::InFlight => {
                info!(
                    "Order {} payment still in-flight (hash {}), skipping",
                    order_id, existing_hash
                );
            }
            DevFeePaymentState::Unknown => {
                warn!(
                    "Order {} payment status unknown (hash {}), skipping to avoid duplicate",
                    order_id, existing_hash
                );
            }
        }
    }
}

// ── Phase 4: Process genuinely new (unclaimed) orders ──────────────────

/// Claim, resolve LNURL invoice, store hash, and send payment for orders
/// that have no existing payment hash.
async fn process_new_dev_fee_payments(
    pool: &SqlitePool,
    ln_client: &mut LndConnector,
    confirmed: &mut HashSet<uuid::Uuid>,
) {
    let unpaid_orders = match find_unpaid_dev_fees(pool).await {
        Ok(orders) => orders,
        Err(e) => {
            error!("Failed to query unpaid dev fee orders: {:?}", e);
            return;
        }
    };
    info!("Found {} orders with unpaid dev fees", unpaid_orders.len());

    for mut order in unpaid_orders {
        let order_id = order.id;

        // STEP 0: Atomically claim this order to prevent duplicate processing.
        let now_ts = Utc::now().timestamp() as u64;
        let pending_marker = format!("PENDING-{}-{}", uuid::Uuid::new_v4(), now_ts);

        match try_claim_order_for_dev_fee(pool, order_id, &pending_marker).await {
            Ok(false) => {
                info!(
                    "Order {} already claimed by another cycle, skipping",
                    order_id
                );
                continue;
            }
            Err(e) => {
                error!(
                    "Failed to claim order {} for dev fee payment: {:?}",
                    order_id, e
                );
                continue;
            }
            Ok(true) => {
                info!("Claimed order {} with marker {}", order_id, pending_marker);
            }
        }

        // STEP 1: Resolve invoice and extract real payment hash.
        info!("Resolving dev fee invoice for order {}", order_id);

        let (payment_request, payment_hash_hex) = match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            resolve_dev_fee_invoice(&order),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                error!(
                    "Failed to resolve dev fee invoice for order {}: {:?}",
                    order_id, e
                );
                release_pending_claim(pool, order_id, &pending_marker).await;
                continue;
            }
            Err(_) => {
                error!(
                    "Dev fee invoice resolution timeout (20s) for order {}",
                    order_id
                );
                release_pending_claim(pool, order_id, &pending_marker).await;
                continue;
            }
        };

        // STEP 2: Store real payment hash BEFORE sending payment.
        // If daemon crashes after this point, Phase 3 will find the hash
        // on the next cycle and verify with LND.
        info!(
            "Storing payment hash {} for order {}",
            payment_hash_hex, order_id
        );
        order.dev_fee_paid = true;
        order.dev_fee_payment_hash = Some(payment_hash_hex.clone());

        let mut order = match order.update(pool).await {
            Err(e) => {
                error!(
                    "Failed to store payment hash for order {}: {:?}",
                    order_id, e
                );
                continue;
            }
            Ok(updated_order) => {
                info!("Order {} marked with real payment hash", order_id);
                updated_order
            }
        };

        // STEP 3: Send payment with pre-resolved invoice.
        match tokio::time::timeout(
            std::time::Duration::from_secs(50),
            send_dev_fee_payment(&order, &payment_request, ln_client),
        )
        .await
        {
            Ok(Ok(payment_hash)) => {
                handle_payment_success(&mut order, pool, confirmed, &payment_hash).await;
            }
            Ok(Err(e)) => {
                handle_payment_failure(&mut order, pool, order_id, &e).await;
            }
            Err(_) => {
                handle_payment_timeout(&mut order, pool, ln_client, confirmed).await;
            }
        }
    }
}

// ── Payment outcome handlers ────────────────────────────────────────────

async fn handle_payment_success(
    order: &mut Order,
    pool: &SqlitePool,
    confirmed: &mut HashSet<uuid::Uuid>,
    payment_hash: &str,
) {
    let order_id = order.id;
    let dev_fee_amount = order.dev_fee;

    if order.dev_fee_payment_hash.as_deref() != Some(payment_hash) {
        warn!(
            "Order {}: LND returned hash '{}' differs from stored hash '{:?}', using LND's value",
            order_id, payment_hash, order.dev_fee_payment_hash
        );
        order.dev_fee_payment_hash = Some(payment_hash.to_string());
    }

    info!("Payment succeeded for order {}, verifying DB", order_id);

    match order.clone().update(pool).await {
        Err(e) => {
            error!(
                "CRITICAL: Dev fee PAID for order {} but DB update FAILED",
                order_id
            );
            error!("   Payment amount: {} sats", dev_fee_amount);
            error!("   Payment hash: {}", payment_hash);
            error!("   Database error: {:?}", e);
            error!("   ACTION REQUIRED: Manual reconciliation");
        }
        Ok(_) => {
            info!("Dev fee payment completed for order {}", order_id);
            info!("   Amount: {} sats, Hash: {}", dev_fee_amount, payment_hash);
            confirmed.insert(order_id);

            if let Ok(verified_order) =
                sqlx::query_as::<_, Order>("SELECT * FROM orders WHERE id = ?")
                    .bind(order_id)
                    .fetch_one(pool)
                    .await
            {
                info!(
                    "VERIFICATION: order_id={}, dev_fee_paid={}, hash={:?}",
                    verified_order.id,
                    verified_order.dev_fee_paid,
                    verified_order.dev_fee_payment_hash
                );

                if let Err(e) = publish_dev_fee_audit_event(&verified_order, payment_hash).await {
                    warn!(
                        "Failed to publish audit event for order {}: {:?}",
                        order_id, e
                    );
                }
            }
        }
    }
}

async fn handle_payment_failure(
    order: &mut Order,
    pool: &SqlitePool,
    order_id: uuid::Uuid,
    e: &MostroError,
) {
    // Do NOT clear the hash. The idempotency check (Phase 3) on the next
    // cycle will verify with LND and clear if truly failed. This prevents
    // a race where "failed" is reported prematurely.
    error!(
        "Dev fee payment failed for order {} - error: {:?}",
        order_id, e
    );
    warn!(
        "Keeping payment hash for order {} to let idempotency check verify on next cycle",
        order_id
    );
    order.dev_fee_paid = false;
    if let Err(db_err) = order.clone().update(pool).await {
        error!(
            "Failed to update order {} after payment failure: {:?}",
            order_id, db_err
        );
    }
}

async fn handle_payment_timeout(
    order: &mut Order,
    pool: &SqlitePool,
    ln_client: &mut LndConnector,
    confirmed: &mut HashSet<uuid::Uuid>,
) {
    let order_id = order.id;
    let dev_fee = order.dev_fee;
    warn!(
        "Dev fee payment timeout (50s) for order {} ({} sats), checking LN status",
        order_id, dev_fee
    );

    match check_dev_fee_payment_status(order, ln_client).await {
        DevFeePaymentState::Succeeded => {
            info!(
                "Payment actually succeeded for order {} despite timeout",
                order_id
            );
            order.dev_fee_paid = true;
            if let Err(e) = order.clone().update(pool).await {
                error!(
                    "Payment succeeded but failed to update DB for order {}: {:?}",
                    order_id, e
                );
            }
            confirmed.insert(order_id);
        }
        DevFeePaymentState::InFlight => {
            warn!(
                "Payment still in-flight for order {}, keeping hash",
                order_id
            );
        }
        DevFeePaymentState::Failed => {
            info!(
                "Payment confirmed failed for order {}, clearing hash for retry",
                order_id
            );
            order.dev_fee_paid = false;
            order.dev_fee_payment_hash = None;
            if let Err(e) = order.clone().update(pool).await {
                error!(
                    "Failed to reset after confirmed failure for order {}: {:?}",
                    order_id, e
                );
            }
        }
        DevFeePaymentState::Unknown => {
            warn!(
                "Cannot determine payment status for order {}, keeping hash to avoid duplicate",
                order_id
            );
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Release a PENDING claim back to NULL using exact marker match (safe release).
async fn release_pending_claim(pool: &SqlitePool, order_id: uuid::Uuid, pending_marker: &str) {
    let _ = sqlx::query(
        "UPDATE orders SET dev_fee_payment_hash = NULL
         WHERE id = ? AND dev_fee_payment_hash = ?",
    )
    .bind(order_id)
    .bind(pending_marker)
    .execute(pool)
    .await;
}

/// Possible states of a dev fee payment after checking the LN node.
enum DevFeePaymentState {
    /// Payment confirmed successful on the LN node.
    Succeeded,
    /// Payment is still in-flight on the LN network.
    InFlight,
    /// Payment definitively failed — safe to retry.
    Failed,
    /// Could not determine status (LN node unreachable, unknown hash, etc.)
    Unknown,
}

/// Check the actual payment status on the LN node for a dev fee payment.
///
/// Returns the current payment state so the caller can decide what to do.
async fn check_dev_fee_payment_status(
    order: &Order,
    ln_client: &mut LndConnector,
) -> DevFeePaymentState {
    use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;

    let payment_hash_str = match &order.dev_fee_payment_hash {
        Some(h) if !h.starts_with("PENDING-") => h.clone(),
        _ => {
            warn!(
                "Order {} has no trackable payment hash, cannot verify LN status",
                order.id
            );
            return DevFeePaymentState::Unknown;
        }
    };

    use nostr_sdk::nostr::hashes::hex::FromHex;
    let payment_hash_bytes: Vec<u8> = match FromHex::from_hex(&payment_hash_str) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(
                "Failed to decode payment hash '{}' for order {}: {}",
                payment_hash_str, order.id, e
            );
            return DevFeePaymentState::Unknown;
        }
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ln_client.check_payment_status(&payment_hash_bytes),
    )
    .await
    {
        Ok(Ok(status)) => match status {
            PaymentStatus::Succeeded => DevFeePaymentState::Succeeded,
            PaymentStatus::InFlight => DevFeePaymentState::InFlight,
            PaymentStatus::Failed => DevFeePaymentState::Failed,
            _ => DevFeePaymentState::Unknown,
        },
        Ok(Err(e)) => {
            warn!(
                "LN status check failed for order {} (hash {}): {:?}",
                order.id, payment_hash_str, e
            );
            DevFeePaymentState::Unknown
        }
        Err(_) => {
            warn!(
                "LN status check timed out for order {} (hash {})",
                order.id, payment_hash_str
            );
            DevFeePaymentState::Unknown
        }
    }
}

/// Atomically claim an order for dev fee processing.
///
/// Uses a SQL UPDATE with a WHERE guard so that only one scheduler cycle
/// can claim a given order.
///
/// Returns `Ok(true)` when the claim succeeds (rows_affected > 0),
/// `Ok(false)` when the order was already claimed (rows_affected == 0),
/// and `Err` on database errors.
pub(crate) async fn try_claim_order_for_dev_fee(
    pool: &SqlitePool,
    order_id: uuid::Uuid,
    pending_marker: &str,
) -> Result<bool, MostroError> {
    let result = sqlx::query(
        "UPDATE orders SET dev_fee_payment_hash = ?
         WHERE id = ? AND dev_fee_paid = 0
           AND (dev_fee_payment_hash IS NULL OR dev_fee_payment_hash = '')",
    )
    .bind(pending_marker)
    .bind(order_id)
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(result.rows_affected() > 0)
}

/// Extract the unix timestamp from a PENDING marker.
///
/// Marker format: `PENDING-{uuid}-{unix_timestamp}`
/// Legacy format: `PENDING-{uuid}` (no timestamp) → returns `None`
///
/// Returns `Some(timestamp)` if a valid unix timestamp is found at the end,
/// `None` otherwise.
fn parse_pending_timestamp(marker: &str) -> Option<u64> {
    let stripped = marker.strip_prefix("PENDING-")?;

    // UUID is exactly 36 chars (8-4-4-4-12 hex digits with dashes).
    // The timestamp follows after the UUID and a separating dash.
    if stripped.len() <= 37 {
        return None;
    }

    if stripped.as_bytes().get(36) != Some(&b'-') {
        return None;
    }

    let ts_str = &stripped[37..];
    ts_str.parse::<u64>().ok().filter(|&ts| ts > 1_000_000_000)
}

// ── Invoice resolution & payment (moved from release.rs) ───────────────

/// Resolve a dev fee LNURL invoice for the given order.
///
/// Contacts the dev fee lightning address, obtains a fresh invoice,
/// decodes it, and returns the payment request and payment hash.
///
/// # Timeouts
/// - LNURL resolution: 15 seconds
pub async fn resolve_dev_fee_invoice(order: &Order) -> Result<(String, String), MostroError> {
    info!(
        "Resolving dev fee invoice for order {} - amount: {} sats to {}",
        order.id, order.dev_fee, DEV_FEE_LIGHTNING_ADDRESS
    );

    if order.dev_fee <= 0 {
        return Err(MostroInternalErr(ServiceError::WrongAmountError));
    }

    let payment_request = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        resolv_ln_address(DEV_FEE_LIGHTNING_ADDRESS, order.dev_fee as u64),
    )
    .await
    .map_err(|_| {
        error!(
            "Dev fee LNURL resolution timeout for order {} ({} sats)",
            order.id, order.dev_fee
        );
        MostroInternalErr(ServiceError::LnAddressParseError)
    })?
    .map_err(|e| {
        error!(
            "Dev fee LNURL resolution failed for order {} ({} sats): {:?}",
            order.id, order.dev_fee, e
        );
        e
    })?;

    if payment_request.is_empty() {
        error!(
            "Dev fee LNURL resolution returned empty invoice for order {} ({} sats)",
            order.id, order.dev_fee
        );
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }

    let invoice = decode_invoice(&payment_request)?;
    let payment_hash_hex = bytes_to_string(invoice.payment_hash().as_ref());

    info!(
        "Resolved dev fee invoice for order {} - hash: {}",
        order.id, payment_hash_hex
    );

    Ok((payment_request, payment_hash_hex))
}

/// Send development fee payment via Lightning Network.
///
/// Sends a pre-resolved invoice payment via LND. The caller must first
/// call [`resolve_dev_fee_invoice`] to obtain the payment request and store
/// the payment hash in the database.
///
/// # Timeouts
/// - send_payment call: 5 seconds
/// - Payment result wait: 25 seconds
/// - Total: 30 seconds maximum
pub async fn send_dev_fee_payment(
    order: &Order,
    payment_request: &str,
    ln_client: &mut LndConnector,
) -> Result<String, MostroError> {
    use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;

    info!(
        "Sending dev fee payment for order {} - amount: {} sats",
        order.id, order.dev_fee
    );

    if order.dev_fee <= 0 {
        return Err(MostroInternalErr(ServiceError::WrongAmountError));
    }

    let (tx, mut rx) = channel(100);

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ln_client.send_payment(payment_request, order.dev_fee, tx),
    )
    .await
    .map_err(|_| {
        error!(
            "Dev fee send_payment timeout for order {} ({} sats)",
            order.id, order.dev_fee
        );
        MostroInternalErr(ServiceError::LnPaymentError(
            "send_payment timeout".to_string(),
        ))
    })?
    .map_err(|e| {
        error!(
            "Dev fee send_payment failed for order {} ({} sats): {:?}",
            order.id, order.dev_fee, e
        );
        e
    })?;

    let payment_result = tokio::time::timeout(std::time::Duration::from_secs(25), async {
        while let Some(msg) = rx.recv().await {
            if let Ok(status) = PaymentStatus::try_from(msg.payment.status) {
                match status {
                    PaymentStatus::Succeeded => {
                        return Ok(msg.payment.payment_hash);
                    }
                    PaymentStatus::Failed => {
                        error!(
                            "Dev fee payment failed for order {} ({} sats) - failure_reason: {}",
                            order.id, order.dev_fee, msg.payment.failure_reason
                        );
                        return Err(MostroInternalErr(ServiceError::LnPaymentError(format!(
                            "payment failed: reason {}",
                            msg.payment.failure_reason
                        ))));
                    }
                    _ => {}
                }
            }
        }
        error!(
            "Dev fee payment channel closed for order {} ({} sats)",
            order.id, order.dev_fee
        );
        Err(MostroInternalErr(ServiceError::LnPaymentError(
            "channel closed".to_string(),
        )))
    })
    .await
    .map_err(|_| {
        error!(
            "Dev fee payment result timeout for order {} ({} sats)",
            order.id, order.dev_fee
        );
        MostroInternalErr(ServiceError::LnPaymentError("result timeout".to_string()))
    })??;

    info!(
        "Dev fee payment succeeded for order {} - amount: {} sats, hash: {}",
        order.id, order.dev_fee, payment_result
    );
    Ok(payment_result)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{parse_pending_timestamp, try_claim_order_for_dev_fee};
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup_orders_db() -> sqlx::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("Failed to create in-memory DB");

        sqlx::query(
            r#"
            CREATE TABLE orders (
                id char(36) primary key not null,
                kind varchar(4) not null default 'buy',
                event_id char(64) not null default '',
                hash char(64),
                preimage char(64),
                creator_pubkey char(64) default '',
                cancel_initiator_pubkey char(64),
                dispute_initiator_pubkey char(64),
                buyer_pubkey char(64),
                master_buyer_pubkey char(64),
                seller_pubkey char(64),
                master_seller_pubkey char(64),
                status varchar(10) not null default 'active',
                price_from_api integer not null default 0,
                premium integer not null default 0,
                payment_method varchar(500) not null default 'cash',
                amount integer not null default 0,
                min_amount integer default 0,
                max_amount integer default 0,
                buyer_dispute integer not null default 0,
                seller_dispute integer not null default 0,
                buyer_cooperativecancel integer not null default 0,
                seller_cooperativecancel integer not null default 0,
                fee integer not null default 0,
                routing_fee integer not null default 0,
                fiat_code varchar(5) not null default 'USD',
                fiat_amount integer not null default 0,
                buyer_invoice text,
                range_parent_id char(36),
                invoice_held_at integer default 0,
                taken_at integer default 0,
                created_at integer not null default 0,
                buyer_sent_rate integer default 0,
                seller_sent_rate integer default 0,
                payment_attempts integer default 0,
                failed_payment integer default 0,
                expires_at integer not null default 0,
                trade_index_seller integer default 0,
                trade_index_buyer integer default 0,
                next_trade_pubkey char(64),
                next_trade_index integer default 0,
                dev_fee integer default 0,
                dev_fee_paid integer not null default 0,
                dev_fee_payment_hash char(64)
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("Failed to create orders table");

        pool
    }

    async fn insert_test_order(
        pool: &sqlx::SqlitePool,
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
                                dev_fee, dev_fee_paid, dev_fee_payment_hash)
            VALUES (?, 'sell', 'evt1', ?, 0, 'cash', 1000, 'USD', 100, 0, 0, ?, ?, ?)
            "#,
        )
        .bind(id)
        .bind(status)
        .bind(dev_fee)
        .bind(dev_fee_paid as i32)
        .bind(dev_fee_payment_hash)
        .execute(pool)
        .await
        .expect("Failed to insert test order");
    }

    #[tokio::test]
    async fn atomic_claim_succeeds_for_unclaimed_order() {
        let pool = setup_orders_db().await;
        let order_id = uuid::Uuid::new_v4();
        insert_test_order(&pool, order_id, "success", 100, false, None).await;

        let claimed = try_claim_order_for_dev_fee(&pool, order_id, "PENDING-test-1234567890")
            .await
            .unwrap();
        assert!(claimed, "Should successfully claim an unclaimed order");

        let hash: Option<String> =
            sqlx::query_scalar("SELECT dev_fee_payment_hash FROM orders WHERE id = ?")
                .bind(order_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(hash.as_deref(), Some("PENDING-test-1234567890"));
    }

    #[tokio::test]
    async fn atomic_claim_fails_for_already_claimed_order() {
        let pool = setup_orders_db().await;
        let order_id = uuid::Uuid::new_v4();
        insert_test_order(
            &pool,
            order_id,
            "success",
            100,
            false,
            Some("PENDING-other-cycle"),
        )
        .await;

        let claimed = try_claim_order_for_dev_fee(&pool, order_id, "PENDING-test-1234567890")
            .await
            .unwrap();
        assert!(
            !claimed,
            "Should not claim an order already claimed by another cycle"
        );

        let hash: Option<String> =
            sqlx::query_scalar("SELECT dev_fee_payment_hash FROM orders WHERE id = ?")
                .bind(order_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(hash.as_deref(), Some("PENDING-other-cycle"));
    }

    #[tokio::test]
    async fn atomic_claim_fails_for_already_paid_order() {
        let pool = setup_orders_db().await;
        let order_id = uuid::Uuid::new_v4();
        insert_test_order(&pool, order_id, "success", 100, true, None).await;

        let claimed = try_claim_order_for_dev_fee(&pool, order_id, "PENDING-test-1234567890")
            .await
            .unwrap();
        assert!(!claimed, "Should not claim an already-paid order");
    }

    #[tokio::test]
    async fn atomic_claim_fails_for_nonexistent_order() {
        let pool = setup_orders_db().await;
        let order_id = uuid::Uuid::new_v4();

        let claimed = try_claim_order_for_dev_fee(&pool, order_id, "PENDING-test-1234567890")
            .await
            .unwrap();
        assert!(!claimed, "Should not claim a nonexistent order");
    }

    #[tokio::test]
    async fn atomic_claim_with_empty_hash_succeeds() {
        let pool = setup_orders_db().await;
        let order_id = uuid::Uuid::new_v4();
        insert_test_order(&pool, order_id, "success", 100, false, Some("")).await;

        let claimed = try_claim_order_for_dev_fee(&pool, order_id, "PENDING-test-1234567890")
            .await
            .unwrap();
        assert!(claimed, "Should claim order with empty string payment hash");
    }

    #[test]
    fn test_parse_new_format_with_uuid() {
        let marker = "PENDING-550e8400-e29b-41d4-a716-446655440000-1707700000";
        assert_eq!(parse_pending_timestamp(marker), Some(1707700000));
    }

    #[test]
    fn test_parse_legacy_format_uuid() {
        let marker = "PENDING-550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(parse_pending_timestamp(marker), None);
    }

    #[test]
    fn test_parse_not_pending() {
        assert_eq!(parse_pending_timestamp("some-random-hash"), None);
        assert_eq!(parse_pending_timestamp(""), None);
    }

    #[test]
    fn test_parse_plain_pending() {
        assert_eq!(parse_pending_timestamp("PENDING"), None);
        assert_eq!(parse_pending_timestamp("PENDING-"), None);
    }

    #[test]
    fn test_parse_invalid_timestamp() {
        let marker = "PENDING-550e8400-e29b-41d4-a716-446655440000-notanumber";
        assert_eq!(parse_pending_timestamp(marker), None);
    }

    #[test]
    fn test_parse_too_small_timestamp() {
        let marker = "PENDING-550e8400-e29b-41d4-a716-446655440000-12345";
        assert_eq!(parse_pending_timestamp(marker), None);
    }

    #[test]
    fn test_parse_current_timestamp() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let marker = format!("PENDING-550e8400-e29b-41d4-a716-446655440000-{}", now);
        assert_eq!(parse_pending_timestamp(&marker), Some(now));
    }
}
