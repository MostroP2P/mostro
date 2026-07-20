//! Cashu escrow lock handler — Track A **TA-1**
//! (see `docs/cashu/02-track-a-lock.md` §4).
//!
//! The Cashu analogue of "the seller funds the escrow": accept the seller's
//! `AddCashuEscrow`, **fully validate** the 2-of-3 token against the mint and
//! the order's trade keys, **atomically** persist it and advance the order
//! (`WaitingPayment → Active`), then notify the buyer to send fiat.
//!
//! Validation ordering matters: **validate fully before mutating any state**,
//! then commit atomically — the same discipline `release_action` applies
//! (compute/verify first, persist second, notify last). All notifications
//! happen **after** the successful compare-and-set, so a validation or
//! persistence failure leaves the order exactly as it was and the seller can
//! retry.
//!
//! Fee collection (Option 2, §4A) is **not** handled here — it lands in TA-1f.

use crate::app::context::AppContext;
use crate::cashu::{cashu_pubkey_from_xonly_hex, Error as CashuError};
use crate::config::settings::Settings;
use crate::db::update_order_cashu_escrow;
use crate::util::{enqueue_order_msg, get_order, update_order_event};
use chrono::Utc;
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

/// Seconds in a day — the escrow locktime floor is configured in days
/// (`cashu.escrow_locktime_days`, §4B) and enforced here in seconds.
const SECONDS_PER_DAY: u64 = 86_400;

/// Map a [`CashuError`] onto the wire-level [`CantDoReason`] the seller sees.
/// A mint that is unreachable/timing out is `CashuMintUnavailable` (retryable);
/// a malformed/mis-valued/wrong-condition token is `InvalidCashuToken`; a bad
/// mint URL is `InvalidMintUrl`.
fn cashu_reason(e: &CashuError) -> CantDoReason {
    match e {
        CashuError::InvalidMintUrl(_) => CantDoReason::InvalidMintUrl,
        CashuError::MintConnection(_) | CashuError::Client(_) => CantDoReason::CashuMintUnavailable,
        CashuError::Token(_) | CashuError::Condition(_) => CantDoReason::InvalidCashuToken,
    }
}

/// Handle a seller's `AddCashuEscrow` submission (Track A §4).
///
/// On success the order advances `WaitingPayment → Active` in one atomic write
/// and both parties are notified with `CashuEscrowLocked` (the buyer's cue to
/// send fiat). Every rejection path returns the matching `CantDoReason` and
/// leaves the order unchanged; a replayed or concurrent submission matches zero
/// rows in the compare-and-set and is a safe idempotent no-op.
pub async fn add_cashu_escrow_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();

    // 1. Resolve the order (and the request id for the seller's ack).
    let order = get_order(&msg, pool).await?;
    let request_id = msg.get_inner_message_kind().request_id;

    // 2. Authorise the sender: only the order's seller trade key may fund the
    //    escrow (same identity-check shape as `release_action`).
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    if seller_pubkey != event.sender {
        return Err(MostroCantDo(CantDoReason::InvalidPeer));
    }

    // 3. The order must be waiting for the seller to fund it.
    order
        .check_status(Status::WaitingPayment)
        .map_err(MostroCantDo)?;

    // 4. Extract the lock proof. `MessageKind::verify()` already guarantees the
    //    payload shape; re-check defensively.
    let proof = match msg.get_inner_message_kind().get_payload() {
        Some(Payload::CashuLockProof(p)) => p.clone(),
        _ => return Err(MostroCantDo(CantDoReason::InvalidCashuToken)),
    };

    // 5. Bind the mint: the node only escrows on its own configured mint. This
    //    is a cheap field pre-check; `verify_escrow_token` (step 7) enforces
    //    the authoritative binding (the token's mint == the configured mint).
    let configured_mint = Settings::get_cashu()
        .map(|c| c.mint_url.clone())
        .ok_or_else(|| {
            MostroInternalErr(ServiceError::UnexpectedError(
                "cashu mode without [cashu] settings".to_string(),
            ))
        })?;
    if proof.mint_url.trim_end_matches('/') != configured_mint.trim_end_matches('/') {
        return Err(MostroCantDo(CantDoReason::InvalidMintUrl));
    }

    // 6. Bind the pubkeys to THIS order. The 2-of-3 must lock to the keys
    //    Mostro already holds for this order, never attacker-chosen keys. We
    //    both reject a proof whose stated keys disagree with the order (a cheap
    //    offline check) AND derive the authoritative `{P_B, P_S, P_M}` from the
    //    order — never from the proof — to hand to the mint validation.
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;
    let mostro_pubkey = my_keys.public_key();
    if proof.buyer_pubkey != buyer_pubkey.to_string()
        || proof.seller_pubkey != seller_pubkey.to_string()
        || proof.mostro_pubkey != mostro_pubkey.to_string()
    {
        return Err(MostroCantDo(CantDoReason::InvalidCashuToken));
    }
    let to_cashu = |hex: String| {
        cashu_pubkey_from_xonly_hex(&hex).map_err(|_| MostroCantDo(CantDoReason::InvalidCashuToken))
    };
    let p_b = to_cashu(buyer_pubkey.to_string())?;
    let p_s = to_cashu(seller_pubkey.to_string())?;
    let p_m = to_cashu(mostro_pubkey.to_string())?;

    // Amount: the escrow token locks the order amount exactly (Option 2 — the
    // Mostro fee is a separate token handled in TA-1f).
    let expected_amount =
        u64::try_from(order.amount).map_err(|_| MostroCantDo(CantDoReason::InvalidAmount))?;
    if expected_amount == 0 {
        return Err(MostroCantDo(CantDoReason::InvalidAmount));
    }

    // 7. Validate the token against the mint: 2-of-3 condition + seller-recovery
    //    locktime floor + mint binding + amount + DLEQ (NUT-12) + unspent
    //    (NUT-07). The floor is `now + cashu.escrow_locktime_days`; the seller
    //    may set a longer locktime, never a shorter one (§4B).
    let cashu_client = ctx.cashu_client().ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(
            "cashu client not connected".to_string(),
        ))
    })?;
    let now = Utc::now().timestamp();
    let locktime_days = Settings::get_cashu()
        .map(|c| c.escrow_locktime_days)
        .unwrap_or(15) as u64;
    let min_locktime = (now as u64).saturating_add(locktime_days.saturating_mul(SECONDS_PER_DAY));
    cashu_client
        .verify_escrow_token(&proof.token, p_b, p_s, p_m, expected_amount, min_locktime)
        .await
        .map_err(|e| MostroCantDo(cashu_reason(&e)))?;

    // 8. Atomically persist the escrow and advance the status in one write. A
    //    `false` return means the status changed concurrently or the escrow is
    //    already locked (replay) — log and return `Ok(())` without notifying
    //    (idempotent; same shape as the `rows_affected() == 0` guard in
    //    `release_action`).
    let locked = update_order_cashu_escrow(
        pool,
        order.id,
        &configured_mint,
        &proof.token,
        now,
        Status::WaitingPayment,
        Status::Active,
    )
    .await?;
    if !locked {
        tracing::info!(
            "cashu lock: compare-and-set matched zero rows for order {} (replay or status moved on) — no-op",
            order.id
        );
        return Ok(());
    }

    // 9. Publish the updated (Active) order event so the public state stays
    //    consistent, mirroring the LN funding path. Best-effort: the lock is
    //    already committed, and a retry would hit the zero-row CAS guard, so a
    //    failure here is logged, never returned.
    match Order::by_id(pool, order.id).await {
        Ok(Some(fresh)) => match update_order_event(my_keys, Status::Active, &fresh).await {
            Ok(updated) => {
                if let Err(e) = updated.update(pool).await {
                    tracing::error!(
                        "cashu lock: failed to persist order event for {}: {e}",
                        order.id
                    );
                }
            }
            Err(e) => tracing::error!(
                "cashu lock: failed to publish order event for {}: {e}",
                order.id
            ),
        },
        Ok(None) => tracing::error!("cashu lock: order {} vanished after lock", order.id),
        Err(e) => tracing::error!("cashu lock: refetch failed for {}: {e}", order.id),
    }

    // 10. Notify both parties. The buyer learns the escrow is live and can send
    //     fiat; the seller gets an ack carrying the request id.
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::CashuEscrowLocked,
        None,
        buyer_pubkey,
        None,
    )
    .await;
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::CashuEscrowLocked,
        None,
        seller_pubkey,
        None,
    )
    .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use nostr_sdk::{Keys, Timestamp};
    use sqlx::SqlitePool;
    use std::sync::Arc;

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

    /// A `WaitingPayment` sell order — the state a taken Cashu order sits in
    /// while it waits for the seller to lock the escrow (§2).
    fn waiting_payment_order(seller: PublicKey, buyer: PublicKey) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::WaitingPayment.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: seller.to_string(),
            seller_pubkey: Some(seller.to_string()),
            master_seller_pubkey: Some(seller.to_string()),
            buyer_pubkey: Some(buyer.to_string()),
            master_buyer_pubkey: Some(buyer.to_string()),
            amount: 21_000,
            fee: 21,
            fiat_amount: 40,
            ..Default::default()
        }
    }

    fn unwrapped_from(sender: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::new_order(None, Some(1), None, Action::AddCashuEscrow, None),
            signature: None,
            sender,
            identity: Keys::generate().public_key(),
            created_at: Timestamp::now(),
        }
    }

    fn lock_message(order_id: uuid::Uuid, payload: Option<Payload>) -> Message {
        Message::new_order(
            Some(order_id),
            Some(1),
            None,
            Action::AddCashuEscrow,
            payload,
        )
    }

    /// `cashu_reason` maps each `CashuError` onto the reason the seller sees:
    /// unreachable mint ⇒ retryable `CashuMintUnavailable`; bad token/condition
    /// ⇒ `InvalidCashuToken`; bad URL ⇒ `InvalidMintUrl`.
    #[test]
    fn cashu_reason_maps_every_error_variant() {
        assert_eq!(
            cashu_reason(&CashuError::InvalidMintUrl("x".into())),
            CantDoReason::InvalidMintUrl
        );
        assert_eq!(
            cashu_reason(&CashuError::MintConnection("x".into())),
            CantDoReason::CashuMintUnavailable
        );
        assert_eq!(
            cashu_reason(&CashuError::Token("x".into())),
            CantDoReason::InvalidCashuToken
        );
        assert_eq!(
            cashu_reason(&CashuError::Condition("x".into())),
            CantDoReason::InvalidCashuToken
        );
    }

    /// Step 2: only the seller trade key may fund the escrow. A submission from
    /// anyone else is rejected with `InvalidPeer` before any mint contact.
    #[tokio::test]
    async fn rejects_sender_that_is_not_the_seller() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = waiting_payment_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        // The buyer (not the seller) tries to lock.
        let event = unwrapped_from(buyer);
        let msg = lock_message(order.id, None);
        let my_keys = Keys::generate();

        let result = add_cashu_escrow_action(&ctx, msg, &event, &my_keys).await;
        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPeer))
        ));
    }

    /// Step 3: the order must be `WaitingPayment`. A lock against any other
    /// status is rejected (the CAS would also refuse it, but we fail early).
    #[tokio::test]
    async fn rejects_order_that_is_not_waiting_payment() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = waiting_payment_order(seller, buyer);
        order.status = Status::Active.to_string();
        let order = order.create(&pool).await.unwrap();
        let event = unwrapped_from(seller);
        let msg = lock_message(order.id, None);
        let my_keys = Keys::generate();

        let result = add_cashu_escrow_action(&ctx, msg, &event, &my_keys).await;
        assert!(matches!(result, Err(MostroCantDo(_))));
    }

    /// Step 4: a message without a `CashuLockProof` payload is rejected with
    /// `InvalidCashuToken` (the seller sent no token to validate).
    #[tokio::test]
    async fn rejects_missing_lock_proof_payload() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = waiting_payment_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        let event = unwrapped_from(seller);
        // A non-CashuLockProof payload (rating) must not be accepted.
        let msg = lock_message(order.id, Some(Payload::RatingUser(5)));
        let my_keys = Keys::generate();

        let result = add_cashu_escrow_action(&ctx, msg, &event, &my_keys).await;
        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidCashuToken))
        ));
        // The order is untouched.
        let db = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(db.status, Status::WaitingPayment.to_string());
        assert!(db.cashu_escrow_token.is_none());
    }
}
