//! Track A — Cashu escrow lock (`Action::AddCashuEscrow`).
//!
//! This is the Cashu analogue of the seller paying the Lightning hold invoice.
//! Instead of Mostro taking custody, the seller swaps unencumbered ecash into a
//! NUT-11 2-of-3 P2PK token (locked to the buyer, seller and Mostro trade/arb
//! pubkeys) on the operator-configured mint and submits it here. Mostro only
//! *validates* it — it never holds more than its single key — then advances the
//! order to `Active` and tells the buyer to send fiat (box 2→3 of the sequence
//! diagram in `docs/CASHU_ESCROW_ARCHITECTURE.md`).

use crate::app::context::AppContext;
use crate::cashu::cashu_pubkey_from_xonly_hex;
use crate::db;
use crate::util::{enqueue_order_msg, get_order, update_order_event};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;

/// Map a [`crate::cashu::Error`] from token validation onto the client-facing
/// [`CantDoReason`]. Mint-comms failures are surfaced distinctly from a bad
/// token so the seller can tell "retry later" from "your token is invalid".
fn cashu_reason(err: &crate::cashu::Error) -> CantDoReason {
    use crate::cashu::Error::*;
    match err {
        InvalidMintUrl(_) => CantDoReason::InvalidMintUrl,
        MintConnection(_) | Client(_) => CantDoReason::CashuMintUnavailable,
        Token(_) | Condition(_) => CantDoReason::InvalidCashuToken,
    }
}

/// Handle a seller's Cashu escrow lock submission.
///
/// Validates the submitted 2-of-3 token against the order and the configured
/// mint, then atomically records the escrow and advances `WaitingPayment →
/// Active`. The whole flow is coordinator-only: no funds move through Mostro.
pub async fn add_cashu_escrow_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    let order = get_order(&msg, pool).await?;
    let request_id = msg.get_inner_message_kind().request_id;

    // The escrow is funded while the order waits for the seller, mirroring the
    // Lightning "waiting for the seller to pay the hold invoice" stage.
    if order.status != Status::WaitingPayment.to_string() {
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

    // Only the seller funds the escrow. Both trade pubkeys are set by the take
    // flow before the order reaches WaitingPayment.
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    if event.sender != seller_pubkey {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // The order amount must be resolved (range/market orders settle their
    // amount before WaitingPayment); the escrow must lock a concrete value.
    if order.amount <= 0 {
        return Err(MostroCantDo(CantDoReason::InvalidParameters));
    }

    // Extract the seller's lock proof. `verify()` already guaranteed the
    // payload shape for `AddCashuEscrow`, but match defensively.
    let proof = match msg.get_inner_message_kind().get_payload() {
        Some(Payload::CashuLockProof(p)) => p.clone(),
        _ => return Err(MostroCantDo(CantDoReason::InvalidParameters)),
    };

    // The Cashu client exists only in Cashu mode; the dispatcher only routes
    // this action there, so its absence is an internal misconfiguration.
    let cashu_client = ctx.cashu_client().ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(
            "AddCashuEscrow dispatched without a Cashu client".to_string(),
        ))
    })?;

    // Derive the authoritative {P_B, P_S, P_M} from the order's trade keys and
    // Mostro's identity key. We never trust the pubkeys the seller *states* in
    // the proof — the token must be locked to the keys Mostro already holds.
    // The order stores trade pubkeys as x-only hex; Mostro's key is its node
    // identity pubkey (also x-only hex via `to_string`).
    let to_cashu = |xonly_hex: &str| {
        cashu_pubkey_from_xonly_hex(xonly_hex).map_err(|e| {
            MostroInternalErr(ServiceError::UnexpectedError(format!(
                "trade pubkey to cashu: {e}"
            )))
        })
    };
    let buyer_hex = order
        .buyer_pubkey
        .clone()
        .ok_or(MostroCantDo(CantDoReason::InvalidPubkey))?;
    let seller_hex = order
        .seller_pubkey
        .clone()
        .ok_or(MostroCantDo(CantDoReason::InvalidPubkey))?;
    let p_b = to_cashu(&buyer_hex)?;
    let p_s = to_cashu(&seller_hex)?;
    let p_m = to_cashu(&my_keys.public_key().to_string())?;

    // Validate: 2-of-3 over {P_B,P_S,P_M}, hosted on our mint, exact amount,
    // and every proof unspent at the mint.
    cashu_client
        .verify_escrow_token(&proof.token, p_b, p_s, p_m, order.amount as u64)
        .await
        .map_err(|e| MostroCantDo(cashu_reason(&e)))?;

    // Persist the escrow and advance to Active in one atomic compare-and-set so
    // a locked token is never invisible (see `db::update_order_cashu_escrow`).
    let locked_at = Timestamp::now().as_secs() as i64;
    let token_mint = cashu_client.mint_url().to_string();
    let locked = db::update_order_cashu_escrow(
        pool,
        order.id,
        &token_mint,
        &proof.token,
        locked_at,
        &Status::WaitingPayment.to_string(),
        &Status::Active.to_string(),
    )
    .await?;
    if !locked {
        // Lost the CAS: the order moved off WaitingPayment, or an escrow was
        // already locked (replay / concurrent submission).
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

    // From here on the escrow is durably locked and the order is `Active` in
    // the DB. Everything below — re-publishing the replaceable event and
    // notifying the parties — is best-effort: a failure must NOT be returned to
    // the seller, because a retry would then hit the CAS guard above and get a
    // confusing `InvalidOrderStatus` for an order that is already escrowed.
    // Log loudly and still confirm the lock so the seller receives
    // `CashuEscrowLocked` whenever the lock persisted. Notification context is
    // taken from the pre-CAS `order` (id and trade indices are immutable
    // through the lock), so the notices fire even if the re-fetch fails.
    let order_id = order.id;
    let trade_index_seller = order.trade_index_seller;
    let trade_index_buyer = order.trade_index_buyer;

    match get_order(&msg, pool).await {
        Ok(active_order) => {
            match update_order_event(my_keys, Status::Active, &active_order).await {
                Ok(order_updated) => {
                    if let Err(e) = order_updated.update(pool).await {
                        tracing::error!(
                            order_id = %order_id,
                            "AddCashuEscrow: escrow locked but persisting the updated order event failed: {e}"
                        );
                    }
                }
                Err(e) => tracing::error!(
                    order_id = %order_id,
                    "AddCashuEscrow: escrow locked but publishing the order event failed: {e}"
                ),
            }
        }
        Err(e) => tracing::error!(
            order_id = %order_id,
            "AddCashuEscrow: escrow locked but re-fetching the order failed: {e}"
        ),
    }

    // Confirm the lock to the seller.
    enqueue_order_msg(
        request_id,
        Some(order_id),
        Action::CashuEscrowLocked,
        None,
        seller_pubkey,
        trade_index_seller,
    )
    .await;

    // Notify the buyer that escrow is locked — they can now send fiat.
    enqueue_order_msg(
        None,
        Some(order_id),
        Action::CashuEscrowLocked,
        None,
        buyer_pubkey,
        trade_index_buyer,
    )
    .await;

    Ok(())
}
