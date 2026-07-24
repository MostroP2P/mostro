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
use crate::db::{cashu_escrow_token_in_use, update_order_cashu_escrow};
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

/// Tell both parties the escrow is live: the buyer learns it can send fiat,
/// the seller gets an ack carrying its request id.
///
/// Shared by the happy path (step 10) and the replay-recovery path (step 3b),
/// which is the whole point of factoring it out — a retry after a lost
/// notification must produce exactly the same pair of messages as the original
/// lock.
async fn notify_escrow_locked(
    order_id: uuid::Uuid,
    buyer_pubkey: PublicKey,
    seller_pubkey: PublicKey,
    request_id: Option<u64>,
) {
    enqueue_order_msg(
        None,
        Some(order_id),
        Action::CashuEscrowLocked,
        None,
        buyer_pubkey,
        None,
    )
    .await;
    enqueue_order_msg(
        request_id,
        Some(order_id),
        Action::CashuEscrowLocked,
        None,
        seller_pubkey,
        None,
    )
    .await;
}

/// Handle a seller's `AddCashuEscrow` submission (Track A §4).
///
/// On success the order advances `WaitingPayment → Active` in one atomic write
/// and both parties are notified with `CashuEscrowLocked` (the buyer's cue to
/// send fiat). Every rejection path returns the matching `CantDoReason` and
/// leaves the order unchanged.
///
/// Replays never write twice. A concurrent submission matches zero rows in the
/// compare-and-set and is a safe no-op; a *sequential* retry of an escrow this
/// handler already locked re-sends the notifications and returns (step 3b), so
/// a crash between the commit and the send queue cannot strand the trade with
/// a funded escrow and an uninformed buyer.
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
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // 3. The order must be waiting for the seller to fund it — with exactly one
    //    exception, handled in 3b below: an order this handler already locked
    //    (escrow stored, status `Active`) is a *replay*, not a stray request.
    //    Rejecting it here would make the documented idempotent retry
    //    unreachable and strand a trade whose notifications were lost between
    //    the commit and the send queue. Any other status — including an
    //    `Active` order with no escrow, or a locked order that has since moved
    //    past `Active` — still falls through to the normal rejection, so a
    //    stale "escrow locked, send fiat" is never replayed onto a trade that
    //    has advanced.
    let replayable =
        order.cashu_escrow_locked_at.is_some() && order.status == Status::Active.to_string();
    if !replayable {
        order
            .check_status(Status::WaitingPayment)
            .map_err(MostroCantDo)?;
    }

    // 4. Extract the lock proof. `MessageKind::verify()` already guarantees the
    //    payload shape; re-check defensively.
    let proof = match msg.get_inner_message_kind().get_payload() {
        Some(Payload::CashuLockProof(p)) => p.clone(),
        _ => return Err(MostroCantDo(CantDoReason::InvalidCashuToken)),
    };

    // 3b. Replay recovery. The order is already locked and `Active`: if this
    //     submission carries the very token we stored, re-send the post-commit
    //     notifications and stop — no mint round-trip, no second write, and the
    //     buyer finally gets its cue to send fiat. A *different* token on an
    //     already-locked order is a second funding attempt, which the escrow
    //     can never honour, so it is rejected outright. Only the seller trade
    //     key (checked in step 2) can reach this path.
    if replayable {
        if order.cashu_escrow_token.as_deref() != Some(proof.token.as_str()) {
            return Err(MostroCantDo(CantDoReason::InvalidCashuToken));
        }
        tracing::info!(
            "cashu lock: replayed AddCashuEscrow for already-locked order {} — re-notifying both parties",
            order.id
        );
        notify_escrow_locked(order.id, buyer_pubkey, seller_pubkey, request_id).await;
        return Ok(());
    }

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

    // 6b. Reject a token already escrowed by another order. The 2-of-3 commits
    //     to `{P_B, P_S, P_M}` — trade keys, not an order id — so the checks
    //     above cannot tell this order apart from another one sharing the same
    //     trade keys, and the mint reports the proofs unspent until the first
    //     redeem: without this guard both orders would validate the same token
    //     and go `Active` against a single redeemable escrow. Checked before
    //     the mint round-trip so the seller gets a clear reason cheaply; the
    //     CAS in step 8 repeats it atomically for the concurrent case.
    if cashu_escrow_token_in_use(pool, &proof.token, order.id).await? {
        tracing::warn!(
            "cashu lock: escrow token for order {} is already locked to another order — rejected",
            order.id
        );
        return Err(MostroCantDo(CantDoReason::InvalidCashuToken));
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
        // Zero rows means one of three things. Two are benign (the status moved
        // on, or a concurrent submission won the race and already locked this
        // order) and stay idempotent no-ops. The third — a concurrent
        // submission that locked this same token onto a *different* order,
        // losing the step-6b race — must surface as a rejection so the seller
        // is not left believing an escrow it can no longer fund is live.
        if cashu_escrow_token_in_use(pool, &proof.token, order.id).await? {
            tracing::warn!(
                "cashu lock: escrow token for order {} was locked to another order concurrently — rejected",
                order.id
            );
            return Err(MostroCantDo(CantDoReason::InvalidCashuToken));
        }
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
    //     fiat; the seller gets an ack carrying the request id. If this send is
    //     lost (crash, dropped queue), the seller's retry replays it via 3b.
    notify_escrow_locked(order.id, buyer_pubkey, seller_pubkey, request_id).await;

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

    /// Stamp the escrow columns straight onto a row, standing in for a lock
    /// this handler already committed (the state a replay arrives against).
    async fn mark_locked(pool: &SqlitePool, id: uuid::Uuid, token: &str, status: Status) {
        sqlx::query(
            "UPDATE orders SET cashu_mint_url = ?1, cashu_escrow_token = ?2,
             cashu_escrow_locked_at = ?3, status = ?4 WHERE id = ?5",
        )
        .bind("https://mint.example.com")
        .bind(token)
        .bind(1700000100_i64)
        .bind(status.to_string())
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }

    /// The escrow columns as stored, to assert a rejected or replayed
    /// submission rewrote nothing.
    async fn escrow_columns(pool: &SqlitePool, id: uuid::Uuid) -> (Option<String>, Option<i64>) {
        sqlx::query_as(
            "SELECT cashu_escrow_token, cashu_escrow_locked_at FROM orders WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn lock_proof(token: &str, buyer: PublicKey, seller: PublicKey, mostro: PublicKey) -> Payload {
        Payload::CashuLockProof(CashuLockProof::new(
            token.to_string(),
            "https://mint.example.com".to_string(),
            buyer.to_string(),
            seller.to_string(),
            mostro.to_string(),
        ))
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

    /// Step 3b: a seller retrying after the lock committed but the
    /// notifications were lost gets the notifications replayed, not a status
    /// rejection — otherwise the escrow is funded and the buyer never learns
    /// to send fiat. The retry must not touch state or contact the mint (this
    /// test runs with no mint and no `[cashu]` settings; reaching step 5 would
    /// fail).
    #[tokio::test]
    async fn replayed_lock_on_locked_order_renotifies_without_rewriting_state() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let my_keys = Keys::generate();
        let order = waiting_payment_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        mark_locked(&pool, order.id, "cashuAlockedtoken", Status::Active).await;

        let event = unwrapped_from(seller);
        let msg = lock_message(
            order.id,
            Some(lock_proof(
                "cashuAlockedtoken",
                buyer,
                seller,
                my_keys.public_key(),
            )),
        );

        let result = add_cashu_escrow_action(&ctx, msg, &event, &my_keys).await;
        assert!(result.is_ok(), "a replay of our own lock must be a no-op");

        let (token, locked_at) = escrow_columns(&pool, order.id).await;
        assert_eq!(token.as_deref(), Some("cashuAlockedtoken"));
        assert_eq!(locked_at, Some(1700000100), "the original lock must stand");
    }

    /// A *different* token on an already-locked order is a second funding
    /// attempt the escrow can never honour — rejected, and the stored escrow
    /// is left alone.
    #[tokio::test]
    async fn rejects_a_second_token_on_an_already_locked_order() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let my_keys = Keys::generate();
        let order = waiting_payment_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        mark_locked(&pool, order.id, "cashuAlockedtoken", Status::Active).await;

        let event = unwrapped_from(seller);
        let msg = lock_message(
            order.id,
            Some(lock_proof(
                "cashuAdifferent",
                buyer,
                seller,
                my_keys.public_key(),
            )),
        );

        let result = add_cashu_escrow_action(&ctx, msg, &event, &my_keys).await;
        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidCashuToken))
        ));

        let (token, locked_at) = escrow_columns(&pool, order.id).await;
        assert_eq!(token.as_deref(), Some("cashuAlockedtoken"));
        assert_eq!(locked_at, Some(1700000100));
    }

    /// The replay path is scoped to `Active`: a locked order whose trade has
    /// moved on must NOT get a stale "escrow locked, send fiat" replayed onto
    /// it — it falls through to the ordinary status rejection.
    #[tokio::test]
    async fn does_not_replay_notifications_for_a_locked_order_past_active() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let my_keys = Keys::generate();
        let order = waiting_payment_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        mark_locked(&pool, order.id, "cashuAlockedtoken", Status::FiatSent).await;

        let event = unwrapped_from(seller);
        let msg = lock_message(
            order.id,
            Some(lock_proof(
                "cashuAlockedtoken",
                buyer,
                seller,
                my_keys.public_key(),
            )),
        );

        let result = add_cashu_escrow_action(&ctx, msg, &event, &my_keys).await;
        assert!(
            matches!(result, Err(MostroCantDo(_))),
            "a locked order past Active must be rejected, not re-notified"
        );
    }
}
