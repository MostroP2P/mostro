use crate::app::bond;
use crate::app::bond::flow::{classify_cancel_error, CancelOutcome};
use crate::app::context::AppContext;
use crate::app::dispute::close_dispute_after_user_resolution;
use crate::db::{edit_pubkeys_order, update_order_to_initial_state};
use crate::lightning::LndConnector;
use crate::util::{enqueue_order_msg, get_order, update_order_event};
use fedimint_tonic_lnd::lnrpc::invoice::InvoiceState;
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use std::str::FromStr;
use tracing::{info, warn};

/// The Lightning capabilities the cancel paths need: cancel an escrow, and
/// first find out whether it is safe to.
pub trait CancelLightning {
    fn cancel_hold_invoice<'a>(
        &'a mut self,
        hash: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MostroError>> + Send + 'a>>;

    /// Current state of the escrow invoice at LND, `None` when the node has no
    /// record of it. See [`LndConnector::lookup_invoice_state`].
    fn lookup_invoice_state<'a>(
        &'a mut self,
        hash: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<InvoiceState>, MostroError>> + Send + 'a,
        >,
    >;
}

impl CancelLightning for LndConnector {
    fn cancel_hold_invoice<'a>(
        &'a mut self,
        hash: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MostroError>> + Send + 'a>>
    {
        Box::pin(async move {
            LndConnector::cancel_hold_invoice(self, hash)
                .await
                .map(|_| ())
        })
    }

    fn lookup_invoice_state<'a>(
        &'a mut self,
        hash: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<InvoiceState>, MostroError>> + Send + 'a,
        >,
    > {
        Box::pin(async move { LndConnector::lookup_invoice_state(self, hash).await })
    }
}

/// What to do with an escrow hold invoice a cancel path is about to void.
#[derive(Debug, PartialEq)]
pub(crate) enum EscrowCancelDecision {
    /// Nothing is locked in — cancel the invoice.
    Cancel,
    /// The seller's HTLC is accepted *right now*: canceling refunds it while
    /// `hold_invoice_paid` is concurrently telling the buyer the payment went
    /// through. Leave the escrow alone and let the trade advance.
    SkipPaid,
    /// LND could not be asked. Skipping only delays a cancel; canceling blind
    /// can refund a live escrow, so this is the safe direction.
    SkipUnknown(String),
}

/// Decide whether an escrow invoice may be canceled, from the order's status
/// and the invoice's state at LND.
///
/// The two waiting states give the same `Accepted` opposite meanings:
///
/// - `waiting-payment` — the seller is supposed *not* to have paid yet. An
///   accepted HTLC means they just did, in the gap between the caller's read
///   and now. That is the race that turns a timeout (or a counterparty
///   cancel) into a refund of a live escrow while the buyer is being told to
///   send fiat. Skip.
/// - `waiting-buyer-invoice` — the seller has already paid by definition, and
///   returning their funds is precisely what canceling here is for. Cancel.
///
/// `Settled` cannot arise in `waiting-payment` (only a release settles, and
/// only from a live trade), but it is treated like `Accepted`: whatever it
/// means, it is not something to void blindly. `Open`, `Canceled` and an
/// invoice LND has no record of all mean no live escrow.
pub(crate) fn classify_escrow_cancel(
    status: Status,
    lookup: Result<Option<InvoiceState>, MostroError>,
) -> EscrowCancelDecision {
    if status != Status::WaitingPayment {
        return EscrowCancelDecision::Cancel;
    }
    match lookup {
        Ok(Some(InvoiceState::Accepted)) | Ok(Some(InvoiceState::Settled)) => {
            EscrowCancelDecision::SkipPaid
        }
        Ok(_) => EscrowCancelDecision::Cancel,
        Err(e) => EscrowCancelDecision::SkipUnknown(e.to_string()),
    }
}

/// [`classify_escrow_cancel`] evaluated against the live node.
pub(crate) async fn decide_escrow_cancel<L: CancelLightning + Send + ?Sized>(
    ln_client: &mut L,
    status: Status,
    hash: &str,
) -> EscrowCancelDecision {
    classify_escrow_cancel(status, ln_client.lookup_invoice_state(hash).await)
}

/// Cancel an escrow hold invoice, treating an invoice LND has already voided
/// as success.
///
/// `cancel_hold_invoice` reports `already canceled` / `not found` as an error,
/// which is a fact rather than a failure — and aborting on it strands the
/// order. Every cancel path here voids the escrow before persisting, so a
/// first attempt that dies in between leaves exactly that shape: the escrow is
/// gone and the order is still live. Without this, the retry that should
/// finish the job (the caller's, or the scheduler's next timeout tick) hits
/// the error again and can never converge.
///
/// [`classify_cancel_error`] is the same classifier the bond module uses for
/// its own idempotent cancels; anything it cannot place confidently stays an
/// error, so a transient LND problem still aborts.
pub(crate) async fn cancel_escrow_idempotent<L: CancelLightning + Send + ?Sized>(
    ln_client: &mut L,
    order_id: uuid::Uuid,
    hash: &str,
) -> Result<(), MostroError> {
    match ln_client.cancel_hold_invoice(hash).await {
        Ok(()) => Ok(()),
        Err(e) => match classify_cancel_error(&e) {
            CancelOutcome::AlreadyDone => {
                info!("Order Id {order_id}: escrow was already void at LND ({e}); continuing");
                Ok(())
            }
            CancelOutcome::Transient => Err(e),
        },
    }
}

/// Reset API-provided quote-derived amounts when republishing an order.
///
/// When an order was created with `price_from_api`, its `amount` and `fee`
/// are derived from a volatile quote. If the order is republished (e.g. after
/// cancellation by one party), we clear those values so that the next publish
/// cycle recalculates them with a fresh price.
fn reset_api_quotes(order: &mut Order) {
    if order.price_from_api {
        order.amount = 0;
        order.fee = 0;
        // Also reset dev fee to ensure fresh recalculation on re-take
        order.dev_fee = 0;
    }
}

/// Notify the order creator that the order has been republished with updated state.
///
/// This is used after certain cancellation flows where the order returns to a
/// publishable state and the creator should see the updated `Status`.
async fn notify_creator(order: &Order, request_id: Option<u64>) -> Result<(), MostroError> {
    // Get creator pubkey
    let creator_pubkey = order.get_creator_pubkey().map_err(MostroInternalErr)?;

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::NewOrder,
        Some(Payload::Order(SmallOrder::from(order.clone()))),
        creator_pubkey,
        None,
    )
    .await;

    Ok(())
}

/// Cancel a cooperative execution
/// Step 2 of a cooperative cancel flow: both parties have signaled intent.
///
/// - Cancels the hold invoice if present (funds go back to seller)
/// - Persists `Status::CooperativelyCanceled`
/// - Publishes a new replaceable nostr event and notifies both parties
async fn cancel_cooperative_execution_step_2<L: CancelLightning + Send>(
    ctx: &AppContext,
    event: &UnwrappedMessage,
    request_id: Option<u64>,
    mut order: Order,
    counterparty_pubkey: String,
    my_keys: &Keys,
    ln_client: &mut L,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Guard: the same party cannot both initiate and confirm the cooperative cancel.
    if let Some(initiator) = &order.cancel_initiator_pubkey {
        if *initiator == event.sender.to_string() {
            // We create a Message
            return Err(MostroCantDo(CantDoReason::InvalidPubkey));
        }
    }

    // Cancel hold invoice if present; if funds were locked, this returns them to the seller.
    if let Some(hash) = &order.hash {
        // We return funds to seller
        cancel_escrow_idempotent(ln_client, order.id, hash).await?;
        info!(
            "Cooperative cancel: Order Id {}: Funds returned to seller",
            &order.id
        );
    }
    order.status = Status::CooperativelyCanceled.to_string();
    // update db
    let order = order
        .clone()
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    // Publish a replaceable nostr event reflecting the new status and persist the mapping.
    update_order_event(my_keys, Status::CooperativelyCanceled, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    // We create a Message for an accepted cooperative cancel and send it to both parties
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::CooperativeCancelAccepted,
        None,
        event.sender,
        None,
    )
    .await;
    let counterparty_pubkey = PublicKey::from_str(&counterparty_pubkey)
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::CooperativeCancelAccepted,
        None,
        counterparty_pubkey,
        None,
    )
    .await;
    info!("Cancel: Order Id {} canceled cooperatively!", order.id);

    // If there was an active dispute on this order, close it since the users
    // resolved the situation themselves via cooperative cancellation.
    close_dispute_after_user_resolution(
        ctx,
        &order,
        DisputeStatus::SellerRefunded,
        my_keys,
        "cooperative cancel",
    )
    .await;

    // Phase 1/6: cooperative cancel releases any taker bond and resolves
    // the maker bond at range close (Phase 6 settle-at-close if earlier
    // slices were slashed, else release; the close helper also covers the
    // non-range maker bond via its non-range branch).
    bond::release_taker_bonds_for_order_or_warn(pool, order.id, "cooperative_cancel").await;
    bond::resolve_range_maker_bond_at_close_or_warn(pool, &order, "cooperative_cancel").await;

    Ok(())
}

/// Step 1 of a cooperative cancel flow: first party signals intent.
///
/// - Records the initiator's pubkey
/// - Notifies both parties so the counterparty can confirm (step 2)
async fn cancel_cooperative_execution_step_1(
    pool: &Pool<Sqlite>,
    event: &UnwrappedMessage,
    mut order: Order,
    counterparty_pubkey: String,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    order.cancel_initiator_pubkey = Some(event.sender.to_string());
    // update db
    let order = order
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    // Notify both parties: initiator sees "initiated by you" and the counterparty sees
    // "initiated by peer".
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::CooperativeCancelInitiatedByYou,
        None,
        event.sender,
        None,
    )
    .await;
    let counterparty_pubkey = PublicKey::from_str(&counterparty_pubkey)
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::CooperativeCancelInitiatedByPeer,
        None,
        counterparty_pubkey,
        None,
    )
    .await;

    Ok(())
}

/// Cancel an order by the taker
/// Cancellation path when the taker cancels a not-yet-active order.
///
/// Under the concurrent-bonds model, this releases **only the sender's
/// own bond** — other concurrent prospective takers' `Requested` bonds
/// keep racing. The order's republish / pubkey reset / quote reset
/// only runs when this was the **last** active bond on the order
/// (no other bonds remain after the release); otherwise the order
/// stays in `Pending` with the surviving bonds still in flight and
/// the cancel is effectively scoped to a per-taker release + message.
async fn cancel_order_by_taker<L: CancelLightning + Send>(
    pool: &Pool<Sqlite>,
    event: &UnwrappedMessage,
    order: Order,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut L,
    taker_pubkey: PublicKey,
) -> Result<(), MostroError> {
    let order_id = order.id;
    let sender_str = event.sender.to_string();

    // Release exactly this taker's bond. If no bond row matches (e.g.
    // legacy non-bond order), fall through to the full taker-cancel
    // flow — that path predates the bond and still works.
    let sender_bond =
        crate::app::bond::db::find_active_bond_by_taker(pool, order_id, &sender_str).await?;
    if let Some(bond) = sender_bond.as_ref() {
        if let Err(e) = bond::release_bond(pool, bond).await {
            warn!(
                bond_id = %bond.id,
                "taker_cancel: failed to release sender's bond: {}", e
            );
        }
    }

    // Look at what's left on the order. If other concurrent takers
    // still have active bonds, do NOT reset the order — they are
    // still racing. Just message the sender that their take is cancelled.
    //
    // Phase 5: scope this to *taker* bonds. Under `apply_to = both` the
    // order also carries a `Locked` maker bond (pubkey != the cancelling
    // taker), which must not count as "another taker still racing" — that
    // would wrongly keep the order in `WaitingTakerBond` and prevent it
    // from dropping back to `Pending` when the last taker backs out.
    let remaining = crate::app::bond::db::find_active_bonds_for_order(pool, order_id).await?;
    let others_remain = remaining
        .iter()
        .any(|b| b.pubkey != sender_str && b.role == crate::app::bond::BondRole::Taker.to_string());
    if others_remain {
        enqueue_order_msg(
            request_id,
            Some(order_id),
            Action::Canceled,
            None,
            event.sender,
            None,
        )
        .await;
        return Ok(());
    }

    // No surviving bonds: run the full reset-and-republish path so
    // the order goes back into the book exactly as before.
    cancel_order_by_taker_inner(
        pool,
        event,
        order,
        my_keys,
        request_id,
        ln_client,
        taker_pubkey,
    )
    .await
}

async fn cancel_order_by_taker_inner<L: CancelLightning + Send>(
    pool: &Pool<Sqlite>,
    event: &UnwrappedMessage,
    mut order: Order,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut L,
    taker_pubkey: PublicKey,
) -> Result<(), MostroError> {
    // Cancel hold invoice if present
    if let Some(hash) = &order.hash {
        cancel_escrow_idempotent(ln_client, order.id, hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }

    //We notify the taker that the order is cancelled
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Canceled,
        None,
        event.sender,
        None,
    )
    .await;

    // Reset api quotes
    reset_api_quotes(&mut order);

    // Update order to initial state and save it to the database
    update_order_to_initial_state(pool, order.id, order.amount, order.fee, order.dev_fee)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Clean stored pubkeys for this order; republish will set them anew.
    let order = edit_pubkeys_order(pool, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    let order_updated = update_order_event(my_keys, Status::Pending, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    info!(
        "{}: Canceled order Id {} republishing order",
        taker_pubkey, order.id
    );

    // Notify the creator about the republished order after the taker-side cancellation flow completes
    notify_creator(&order_updated, request_id).await?;

    Ok(())
}

/// Cancel an order by the maker
/// Cancellation path when the maker cancels a not-yet-active order.
///
/// - Publishes `Status::Canceled` and persists it
/// - Cancels any hold invoice
/// - Notifies both parties
async fn cancel_order_by_maker<L: CancelLightning + Send>(
    pool: &Pool<Sqlite>,
    event: &UnwrappedMessage,
    order: Order,
    taker_pubkey: PublicKey,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut L,
) -> Result<(), MostroError> {
    // Void the escrow *before* persisting the cancel. On a cancel failure `?`
    // returns with the order untouched, so the caller retries against a state
    // that still matches the HTLC. Persisting first left a canceled order
    // whose hold invoice was still live at LND — payable for as long as its
    // expiry allows, and cleaned up by nothing: `find_held_invoices` and the
    // escrow-deadline guardian both ignore canceled orders, so the seller's
    // funds would sit locked until LND itself voided the invoice near the
    // CLTV horizon. Same ordering, and the same reasoning, as the scheduler's
    // timeout path and the taker branch above.
    if let Some(hash) = &order.hash {
        cancel_escrow_idempotent(ln_client, order.id, hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }
    // We publish a new replaceable kind nostr event with the status updated.
    // A failure here must surface instead of being skipped: the escrow is
    // already void above, so silently dropping the write left the order live
    // behind a dead escrow *and* told both parties it was canceled. Note that
    // a relay rejection is not this branch — `update_order_event` queues those
    // for republish and returns `Ok` — so this only fires when the event could
    // not be built at all. Mirrors the taker branch and `hold_invoice_paid`.
    let order_updated = update_order_event(my_keys, Status::Canceled, &order).await?;
    order_updated
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Canceled,
        None,
        event.sender,
        None,
    )
    .await;
    //We notify the taker that the order was cancelled
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::Canceled,
        None,
        taker_pubkey,
        None,
    )
    .await;

    // Phase 1/6: maker cancelled before the trade went active — release any
    // taker bond that had already been locked, and resolve the maker bond at
    // range close (release when no slice was slashed; settle-at-close
    // otherwise).
    bond::release_taker_bonds_for_order_or_warn(pool, order.id, "maker_cancel").await;
    bond::resolve_range_maker_bond_at_close_or_warn(pool, &order, "maker_cancel").await;

    Ok(())
}

/// Cancel a `Pending` order by the maker before it becomes active.
///
/// This updates the replaceable event to `Status::Canceled`, persists it, and
/// notifies the maker. No invoice is involved yet in this state.
async fn cancel_pending_order_from_maker(
    pool: &Pool<Sqlite>,
    event: &UnwrappedMessage,
    order: &mut Order,
    my_keys: &Keys,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    // Validates if this user is the order creator
    order
        .sent_from_maker(event.sender)
        .map_err(|_| MostroCantDo(CantDoReason::IsNotYourOrder))?;
    // Publish a replaceable nostr event with updated status and persist it
    // with a compare-and-swap: a taker's take (or bond promotion) can commit
    // while the cancel event is being published, and a stale full-row write
    // would clobber the promoted taker context and escrow material — or
    // resurrect the row the take just moved on from. Losing the CAS means
    // the take won, so the cancel is too late.
    match update_order_event(my_keys, Status::Canceled, order).await {
        Ok(order_updated) => {
            let won = crate::db::cas_pretrade_order_status(
                pool,
                order_updated.id,
                Status::Canceled,
                &order_updated.event_id,
            )
            .await?;
            if !won {
                crate::util::republish_winning_state_after_cas_miss(
                    pool,
                    my_keys,
                    order_updated.id,
                )
                .await;
                return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
            }
        }
        Err(e) => {
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                e.to_string(),
            )));
        }
    }
    // We create a Message for cancel
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Canceled,
        None,
        event.sender,
        None,
    )
    .await;
    // Phase 1: a maker cancelling a still-Pending order may be racing
    // with a taker who just locked (or only requested) a bond. Notify
    // every bonded taker so they don't keep waiting on a cancelled
    // order, and release the bonds so they're made whole. The bond
    // pubkey is the canonical source of who has a stake here — for a
    // fresh Pending order with no taker yet, the lookup returns empty
    // and this is a no-op.
    //
    // A DB error here must not silently drop bonded-taker notifications:
    // log it with order context, then still run the bond release below
    // so cleanup happens regardless of the lookup outcome.
    match crate::app::bond::db::find_active_bonds_for_order(pool, order.id).await {
        Ok(active_bonds) => {
            for active in active_bonds.iter() {
                if let Ok(taker_pk) = PublicKey::from_str(&active.pubkey) {
                    if taker_pk != event.sender {
                        enqueue_order_msg(
                            None,
                            Some(order.id),
                            Action::Canceled,
                            None,
                            taker_pk,
                            None,
                        )
                        .await;
                    }
                }
            }
        }
        Err(err) => {
            warn!(
                order_id = %order.id,
                "pending_maker_cancel: failed to look up active bonds for taker notification: {}",
                err
            );
        }
    }
    bond::release_taker_bonds_for_order_or_warn(pool, order.id, "pending_maker_cancel").await;
    bond::resolve_range_maker_bond_at_close_or_warn(pool, order, "pending_maker_cancel").await;
    Ok(())
}

/// Cancel action entry point using dependency-injected context.
///
/// The database connection pool and other dependencies are extracted from `ctx`.
/// Internal routing logic is delegated to `cancel_action_generic`.
pub async fn cancel_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    cancel_action_generic(ctx, msg, event, my_keys, ln_client).await
}

async fn cancel_action_generic<L: CancelLightning + Send>(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
    ln_client: &mut L,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order id
    let mut order = get_order(&msg, pool).await?;

    // Short-circuit if already canceled in any terminal-cancel state.
    if order.check_status(Status::Canceled).is_ok()
        || order.check_status(Status::CooperativelyCanceled).is_ok()
        || order.check_status(Status::CanceledByAdmin).is_ok()
    {
        return Err(MostroCantDo(CantDoReason::OrderAlreadyCanceled));
    }

    // Pending / WaitingTakerBond: maker can revert to Canceled state and
    // republish without cooperative steps. Phase 1.5 parks pre-trade
    // orders at `WaitingTakerBond` while a taker is mid-bond; per
    // `docs/ANTI_ABUSE_BOND.md` §6.5.1, both statuses must route
    // through the same pre-trade cancel logic. Without this widening
    // the daemon would fall through to `NotAllowedByStatus` for every
    // cancel during the bond window.
    if order.check_status(Status::Pending).is_ok()
        || order.check_status(Status::WaitingTakerBond).is_ok()
    {
        if order.sent_from_maker(event.sender).is_ok() {
            cancel_pending_order_from_maker(pool, event, &mut order, my_keys, request_id).await?;
            return Ok(());
        }
        // Phase 1: a taker who took the order but hasn't paid the bond
        // yet leaves the order in `Pending` (the taker fields are
        // populated; the bond row sits in `Requested`). Allow that taker
        // to back out — release the bond, clear the taker fields, and
        // republish the order so other takers can take it.
        //
        // Prefer matching `event.sender` against an active bond row
        // (the canonical signal). A transient DB failure on that
        // lookup must not block a legitimate taker self-cancel: log
        // it and fall back to the in-memory taker pubkey on the order
        // (whichever side does not match `creator_pubkey`). For a
        // fresh Pending order with no taker yet, neither check
        // matches and we still return `IsNotYourOrder`.
        let sender_str = event.sender.to_string();
        let bond_match =
            match crate::app::bond::db::find_active_bonds_for_order(pool, order.id).await {
                Ok(active_bonds) => active_bonds.iter().any(|b| b.pubkey == sender_str),
                Err(e) => {
                    warn!(
                        order_id = %order.id,
                        "cancel: bond lookup failed for pending taker self-cancel: {}", e
                    );
                    false
                }
            };
        let order_taker_match = order
            .buyer_pubkey
            .as_deref()
            .is_some_and(|p| p == sender_str && p != order.creator_pubkey)
            || order
                .seller_pubkey
                .as_deref()
                .is_some_and(|p| p == sender_str && p != order.creator_pubkey);
        if bond_match || order_taker_match {
            cancel_order_by_taker(
                pool,
                event,
                order,
                my_keys,
                request_id,
                ln_client,
                event.sender,
            )
            .await?;
            return Ok(());
        }
        return Err(MostroCantDo(CantDoReason::IsNotYourOrder));
    }

    // Do the appropriate cancellation flow based on the order status
    // Route to the appropriate cancellation flow based on active vs not-active states.
    match order.get_order_status().map_err(MostroInternalErr)? {
        Status::WaitingPayment | Status::WaitingBuyerInvoice => {
            cancel_not_active_order(pool, event, order, my_keys, request_id, ln_client).await?
        }
        Status::Active | Status::FiatSent | Status::Dispute => {
            cancel_active_order(ctx, event, order, my_keys, request_id, ln_client).await?
        }
        _ => return Err(MostroCantDo(CantDoReason::NotAllowedByStatus)),
    }

    Ok(())
}

/// Cancellation router for active trades.
///
/// Marks which side initiated the cooperative cancel and either starts the flow
/// (step 1) or completes it (step 2) when both sides have acknowledged.
async fn cancel_active_order<L: CancelLightning + Send>(
    ctx: &AppContext,
    event: &UnwrappedMessage,
    mut order: Order,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut L,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get seller and buyer pubkey
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Only the trade counterparties may drive this flow; without this check a
    // third party falls into the `else` branch below and is treated as the seller.
    if event.sender != buyer_pubkey && event.sender != seller_pubkey {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    let counterparty_pubkey: String;
    if buyer_pubkey == event.sender {
        order.buyer_cooperativecancel = true;
        counterparty_pubkey = seller_pubkey.to_string();
    } else {
        order.seller_cooperativecancel = true;
        counterparty_pubkey = buyer_pubkey.to_string();
    }

    // If there is already an initiator recorded, this call becomes the confirmation (step 2).
    match order.cancel_initiator_pubkey.as_deref() {
        Some(initiator) => {
            if initiator != counterparty_pubkey {
                return Err(MostroCantDo(CantDoReason::InvalidPubkey));
            }
            cancel_cooperative_execution_step_2(
                ctx,
                event,
                request_id,
                order,
                counterparty_pubkey,
                my_keys,
                ln_client,
            )
            .await?;
        }
        None => {
            cancel_cooperative_execution_step_1(
                pool,
                event,
                order,
                counterparty_pubkey,
                request_id,
            )
            .await?;
        }
    }
    Ok(())
}

/// Cancellation router for not-yet-active trades.
///
/// If the maker sent the event, run the maker path; otherwise, only the taker
/// can cancel. This ensures the correct party authorization for early cancels.
async fn cancel_not_active_order<L: CancelLightning + Send>(
    pool: &Pool<Sqlite>,
    event: &UnwrappedMessage,
    order: Order,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut L,
) -> Result<(), MostroError> {
    // Get seller and buyer pubkey
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Get order taker pubkey
    let taker_pubkey = if order.creator_pubkey == seller_pubkey.to_string() {
        buyer_pubkey
    } else if order.creator_pubkey == buyer_pubkey.to_string() {
        seller_pubkey
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidPubkey));
    };

    // Resolve the caller's role *before* the escrow guard below, which talks
    // to LND: a sender who is neither party must be rejected as such, without
    // costing a lookup RPC and without learning from the error whether the
    // escrow is funded.
    let sender_is_maker = order.sent_from_maker(event.sender).is_ok();
    if !sender_is_maker && event.sender != taker_pubkey {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // Never void an escrow that is already funded. Both branches below cancel
    // the hold invoice, and in `waiting-payment` an accepted HTLC means the
    // seller paid between this handler's read and now — refunding them here
    // would leave the buyer sending fiat against nothing, since
    // `hold_invoice_paid` is concurrently telling them the payment landed.
    if let Some(hash) = order.hash.as_deref() {
        let status = order.get_order_status().map_err(MostroInternalErr)?;
        match decide_escrow_cancel(ln_client, status, hash).await {
            EscrowCancelDecision::Cancel => {}
            EscrowCancelDecision::SkipPaid => {
                warn!(
                    "Order Id {}: refusing to cancel — the seller's escrow payment just landed",
                    order.id
                );
                return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
            }
            EscrowCancelDecision::SkipUnknown(cause) => {
                warn!(
                    "Order Id {}: could not read the escrow state before canceling ({cause}); rejecting so the caller retries",
                    order.id
                );
                return Err(MostroInternalErr(ServiceError::LnNodeError(cause)));
            }
        }
    }

    if sender_is_maker {
        cancel_order_by_maker(
            pool,
            event,
            order,
            taker_pubkey,
            my_keys,
            request_id,
            ln_client,
        )
        .await?;
    } else {
        cancel_order_by_taker(
            pool,
            event,
            order,
            my_keys,
            request_id,
            ln_client,
            taker_pubkey,
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use mostro_core::db::Crud;
    use nostr_sdk::prelude::{Keys, Timestamp};
    use sqlx::SqlitePool;
    use std::sync::Arc;

    /// Build an `UnwrappedMessage` whose trade key (rumor author / `sender`)
    /// is `pubkey`. The identity key is generated separately so the fixture
    /// reflects the dual-key flow: handlers that gate on `sender` see the
    /// caller; handlers that gate on `identity` see an unrelated key.
    fn create_unwrapped_message_with_pubkey(pubkey: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::Order(MessageKind::new(
                Some(uuid::Uuid::new_v4()),
                Some(1),
                None,
                Action::Cancel,
                None,
            )),
            signature: None,
            sender: pubkey,
            identity: Keys::generate().public_key(),
            created_at: Timestamp::now(),
        }
    }

    fn create_pending_order(maker_pubkey: PublicKey, taker_pubkey: PublicKey) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::Pending.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: maker_pubkey.to_string(),
            seller_pubkey: Some(maker_pubkey.to_string()),
            buyer_pubkey: Some(taker_pubkey.to_string()),
            amount: 21_000,
            fee: 21,
            dev_fee: 1,
            ..Default::default()
        }
    }

    #[test]
    fn reset_api_quotes_resets_amount_fee_and_dev_fee_only_when_api_priced() {
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut api_order = create_pending_order(maker, taker);
        api_order.price_from_api = true;
        reset_api_quotes(&mut api_order);
        assert_eq!(api_order.amount, 0);
        assert_eq!(api_order.fee, 0);
        assert_eq!(api_order.dev_fee, 0);

        let mut fixed_price_order = create_pending_order(maker, taker);
        fixed_price_order.price_from_api = false;
        let original = (
            fixed_price_order.amount,
            fixed_price_order.fee,
            fixed_price_order.dev_fee,
        );
        reset_api_quotes(&mut fixed_price_order);
        assert_eq!(
            (
                fixed_price_order.amount,
                fixed_price_order.fee,
                fixed_price_order.dev_fee
            ),
            original
        );
    }

    struct StubLnClient;

    impl CancelLightning for StubLnClient {
        fn cancel_hold_invoice<'a>(
            &'a mut self,
            _hash: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MostroError>> + Send + 'a>>
        {
            Box::pin(async move { Ok(()) })
        }

        /// Unpaid escrow: the state every existing cancel test assumes.
        fn lookup_invoice_state<'a>(
            &'a mut self,
            _hash: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<InvoiceState>, MostroError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(Some(InvoiceState::Open)) })
        }
    }

    /// Escrow stub that reports a chosen invoice state and records whether the
    /// hold invoice was canceled, so a test can assert the escrow was *not*
    /// touched — the whole point of the guard.
    struct StubEscrowLnClient {
        /// `None` models an invoice LND has no record of.
        state: Option<InvoiceState>,
        fail_lookup: bool,
        fail_cancel: Option<String>,
        canceled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        looked_up: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl StubEscrowLnClient {
        fn reporting(state: Option<InvoiceState>) -> Self {
            Self {
                state,
                fail_lookup: false,
                fail_cancel: None,
                canceled: Default::default(),
                looked_up: Default::default(),
            }
        }

        fn unreachable() -> Self {
            Self {
                state: None,
                fail_lookup: true,
                fail_cancel: None,
                canceled: Default::default(),
                looked_up: Default::default(),
            }
        }

        /// Unpaid escrow whose cancel LND refuses with a transient error.
        fn refusing_cancel() -> Self {
            Self {
                state: Some(InvoiceState::Open),
                fail_lookup: false,
                fail_cancel: Some("cancel refused".to_string()),
                canceled: Default::default(),
                looked_up: Default::default(),
            }
        }

        /// LND reports the invoice as already void — a fact, not a failure.
        fn already_canceled() -> Self {
            Self {
                state: Some(InvoiceState::Canceled),
                fail_lookup: false,
                fail_cancel: Some(
                    "code=Unknown message=invoice with that hash already canceled".to_string(),
                ),
                canceled: Default::default(),
                looked_up: Default::default(),
            }
        }

        fn escrow_was_canceled(&self) -> bool {
            self.canceled.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn escrow_was_looked_up(&self) -> bool {
            self.looked_up.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl CancelLightning for StubEscrowLnClient {
        fn cancel_hold_invoice<'a>(
            &'a mut self,
            _hash: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MostroError>> + Send + 'a>>
        {
            let canceled = self.canceled.clone();
            let fail = self.fail_cancel.clone();
            Box::pin(async move {
                if let Some(cause) = fail {
                    return Err(MostroInternalErr(ServiceError::LnNodeError(cause)));
                }
                canceled.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }

        fn lookup_invoice_state<'a>(
            &'a mut self,
            _hash: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<InvoiceState>, MostroError>>
                    + Send
                    + 'a,
            >,
        > {
            let (state, fail) = (self.state, self.fail_lookup);
            let looked_up = self.looked_up.clone();
            Box::pin(async move {
                looked_up.store(true, std::sync::atomic::Ordering::SeqCst);
                if fail {
                    Err(MostroInternalErr(ServiceError::LnNodeError(
                        "node unreachable".to_string(),
                    )))
                } else {
                    Ok(state)
                }
            })
        }
    }

    // ---------------------------------------------------------------
    // classify_escrow_cancel
    // ---------------------------------------------------------------

    /// The invariant the whole guard exists for: an accepted HTLC on an order
    /// that is still waiting for the seller's payment means they paid just
    /// now, so canceling would refund a live escrow.
    #[test]
    fn escrow_cancel_skips_a_funded_waiting_payment_escrow() {
        for state in [InvoiceState::Accepted, InvoiceState::Settled] {
            assert_eq!(
                classify_escrow_cancel(Status::WaitingPayment, Ok(Some(state))),
                EscrowCancelDecision::SkipPaid,
                "{state:?} must not be voided"
            );
        }
    }

    /// Nothing locked in — including an invoice LND no longer has a record of,
    /// which cannot be refunding anyone.
    #[test]
    fn escrow_cancel_proceeds_when_nothing_is_locked_in() {
        for lookup in [
            Ok(Some(InvoiceState::Open)),
            Ok(Some(InvoiceState::Canceled)),
            Ok(None),
        ] {
            assert_eq!(
                classify_escrow_cancel(Status::WaitingPayment, lookup),
                EscrowCancelDecision::Cancel
            );
        }
    }

    /// Delaying a cancel is recoverable; refunding a live escrow is not.
    #[test]
    fn escrow_cancel_skips_when_lnd_cannot_be_asked() {
        let err = MostroInternalErr(ServiceError::LnNodeError("boom".to_string()));
        assert!(matches!(
            classify_escrow_cancel(Status::WaitingPayment, Err(err)),
            EscrowCancelDecision::SkipUnknown(_)
        ));
    }

    /// The asymmetry that makes the guard status-keyed: in
    /// `waiting-buyer-invoice` the seller has paid by definition, and handing
    /// their funds back is exactly what the cancel is for.
    #[test]
    fn escrow_cancel_still_refunds_the_seller_in_waiting_buyer_invoice() {
        assert_eq!(
            classify_escrow_cancel(
                Status::WaitingBuyerInvoice,
                Ok(Some(InvoiceState::Accepted))
            ),
            EscrowCancelDecision::Cancel
        );
    }

    #[tokio::test]
    async fn cancel_action_with_ctx_rejects_non_creator_for_pending_order() {
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        sqlx::migrate!("./migrations")
            .run(pool.as_ref())
            .await
            .unwrap();
        let ctx = TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .build();

        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();
        let order = create_pending_order(maker, taker)
            .create(ctx.pool())
            .await
            .unwrap();

        // Event is sent by a third party (neither maker nor taker) to trigger auth guard.
        let intruder = Keys::generate().public_key();
        let event = create_unwrapped_message_with_pubkey(intruder);

        let msg = Message::new_order(Some(order.id), Some(1), None, Action::Cancel, None);
        let my_keys = Keys::generate();
        let mut ln_client = StubLnClient;

        let result = cancel_action_generic(&ctx, msg, &event, &my_keys, &mut ln_client).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::IsNotYourOrder))
        ));
    }

    /// Regression test for the stale full-row write clobber: the maker's
    /// cancel holds a pre-promotion snapshot (no taker context) while the
    /// bond subscriber's promotion has already landed on the row. The
    /// cancel must move `status`/`event_id` only — never NULL the promoted
    /// columns.
    #[tokio::test]
    async fn pending_maker_cancel_preserves_concurrently_promoted_taker_context() {
        set_global_config();
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();
        // The maker's stale snapshot: read before the taker's bond locked.
        let mut snapshot = create_pending_order(maker, taker);
        snapshot.buyer_pubkey = None;
        snapshot.master_buyer_pubkey = None;
        let snapshot = snapshot.create(&pool).await.unwrap();

        // Externally apply what the bond subscriber's promotion writes
        // (status still pre-trade: the escrow is not stored yet).
        sqlx::query(
            "UPDATE orders SET status = 'waiting-taker-bond', buyer_pubkey = ?1, \
             master_buyer_pubkey = ?2 WHERE id = ?3",
        )
        .bind(taker.to_string())
        .bind(taker.to_string())
        .bind(snapshot.id)
        .execute(&pool)
        .await
        .unwrap();

        let event = create_unwrapped_message_with_pubkey(maker);
        let mut stale = snapshot.clone();
        cancel_pending_order_from_maker(&pool, &event, &mut stale, &Keys::generate(), None)
            .await
            .unwrap();

        let after = Order::by_id(&pool, snapshot.id).await.unwrap().unwrap();
        assert_eq!(after.status, Status::Canceled.to_string());
        assert_eq!(
            after.buyer_pubkey.as_deref(),
            Some(taker.to_string().as_str()),
            "the promoted taker context must survive the maker's cancel"
        );
        assert_eq!(
            after.master_buyer_pubkey.as_deref(),
            Some(taker.to_string().as_str())
        );
    }

    /// The mirror image: once the take committed past the pre-trade window
    /// (escrow stored, `waiting-payment`), the maker's pending-cancel must
    /// lose instead of reverting the row.
    #[tokio::test]
    async fn pending_maker_cancel_loses_once_the_take_committed() {
        set_global_config();
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();
        let snapshot = create_pending_order(maker, taker)
            .create(&pool)
            .await
            .unwrap();
        // The take committed: escrow stored, status moved on.
        sqlx::query("UPDATE orders SET status = 'waiting-payment', hash = ?1 WHERE id = ?2")
            .bind("ab".repeat(32))
            .bind(snapshot.id)
            .execute(&pool)
            .await
            .unwrap();

        let event = create_unwrapped_message_with_pubkey(maker);
        let mut stale = snapshot.clone();
        let result =
            cancel_pending_order_from_maker(&pool, &event, &mut stale, &Keys::generate(), None)
                .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::NotAllowedByStatus))
        ));
        let after = Order::by_id(&pool, snapshot.id).await.unwrap().unwrap();
        assert_eq!(after.status, Status::WaitingPayment.to_string());
        assert_eq!(after.hash.as_deref(), Some("ab".repeat(32).as_str()));
    }

    /// Phase 1 fix: a taker who took a `Pending` order but hasn't paid
    /// the bond yet must be able to cancel and back out, even though
    /// the order is still `Pending`. Before this fix, `cancel_action`
    /// routed every cancel on a `Pending` order through the maker path
    /// and returned `IsNotYourOrder` for the bonded taker.
    ///
    /// We assert the routing change at the *decision* layer: an active
    /// bond row whose `pubkey` matches `event.sender` switches the
    /// cancel out of the maker-only path. The full cancel side-effects
    /// (`update_order_event`, `notify_creator`) reach into globals
    /// (`get_db_pool`, etc.) that aren't initialized in unit tests, so
    /// they're covered by integration tests rather than asserted here.
    #[tokio::test]
    async fn pending_taker_with_active_bond_is_not_routed_as_intruder() {
        use crate::app::bond::db::find_active_bonds_for_order;
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        sqlx::migrate!("./migrations")
            .run(pool.as_ref())
            .await
            .unwrap();

        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();
        let order = create_pending_order(maker, taker)
            .create(pool.as_ref())
            .await
            .unwrap();

        // Insert a Requested bond row whose pubkey matches the taker's.
        let mut bond = crate::app::bond::Bond::new_requested(
            order.id,
            taker.to_string(),
            crate::app::bond::BondRole::Taker,
            1_500,
        );
        bond.hash = None;
        bond.create(pool.as_ref()).await.unwrap();

        // Sanity: the helper finds the bond by sender match — this is
        // exactly the predicate `cancel_action_generic` uses to decide
        // whether to route to the taker-cancel path.
        let active = find_active_bonds_for_order(pool.as_ref(), order.id)
            .await
            .unwrap();
        let sender_str = taker.to_string();
        assert!(
            active.iter().any(|b| b.pubkey == sender_str),
            "the taker must be recognised as a bonded sender"
        );

        // And the intruder (non-maker, no bond) must still NOT match,
        // so the routing falls through to `IsNotYourOrder`.
        let intruder = Keys::generate().public_key();
        let intruder_str = intruder.to_string();
        assert!(
            !active.iter().any(|b| b.pubkey == intruder_str),
            "an intruder with no bond row must not be routed to the taker-cancel path"
        );
    }

    async fn setup_pool() -> Arc<SqlitePool> {
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        sqlx::migrate!("./migrations")
            .run(pool.as_ref())
            .await
            .unwrap();
        pool
    }

    fn build_ctx(pool: Arc<SqlitePool>) -> AppContext {
        TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .build()
    }

    /// Publishing paths (`update_order_event`) read the global config
    /// (`Settings::get_nostr` / `get_mostro` / `get_expiration`). The
    /// `OnceLock` may already be set by a concurrent test — that is fine
    /// because every unit test uses the same `test_settings()` values.
    fn set_global_config() {
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
    }

    /// The republish-to-`Pending` path (`update_order_event` with target
    /// `Status::Pending`) additionally calls `get_db_pool()`, which panics
    /// unless the global `DB_POOL` is set. Another test may have won the
    /// race with a different pool; tests relying on this therefore pin
    /// `master_*_pubkey == trade pubkey` so the rating lookup falls back
    /// to `(0.0, 0, 0)` deterministically regardless of which pool won.
    fn set_global_db_pool(pool: &Arc<SqlitePool>) {
        let _ = crate::config::DB_POOL.set(pool.clone());
    }

    fn cancel_msg(order_id: uuid::Uuid) -> Message {
        Message::new_order(Some(order_id), Some(1), None, Action::Cancel, None)
    }

    /// Actions queued for `destination` on the process-global queue.
    /// Other tests push to the same queue concurrently, so callers must
    /// only assert on destinations built from this test's fresh keys.
    async fn queued_actions_for(destination: PublicKey) -> Vec<Action> {
        crate::config::MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(_, pk)| *pk == destination)
            .map(|(m, _)| m.get_inner_message_kind().action.clone())
            .collect()
    }

    async fn order_by_id(pool: &SqlitePool, id: uuid::Uuid) -> Order {
        Order::by_id(pool, id).await.unwrap().unwrap()
    }

    async fn insert_requested_taker_bond(
        pool: &SqlitePool,
        order_id: uuid::Uuid,
        pubkey: &PublicKey,
    ) {
        // `hash: None` keeps `release_bond` off the LND connect path.
        let bond = crate::app::bond::Bond::new_requested(
            order_id,
            pubkey.to_string(),
            crate::app::bond::BondRole::Taker,
            1_000,
        );
        bond.create(pool).await.unwrap();
    }

    #[tokio::test]
    async fn cancel_action_rejects_orders_already_in_terminal_cancel_state() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();
        let my_keys = Keys::generate();
        let mut ln_client = StubLnClient;

        for status in [
            Status::Canceled,
            Status::CooperativelyCanceled,
            Status::CanceledByAdmin,
        ] {
            let mut order = create_pending_order(maker, taker);
            order.status = status.to_string();
            let order = order.create(ctx.pool()).await.unwrap();
            let event = create_unwrapped_message_with_pubkey(maker);

            let result =
                cancel_action_generic(&ctx, cancel_msg(order.id), &event, &my_keys, &mut ln_client)
                    .await;

            assert!(
                matches!(
                    result,
                    Err(MostroCantDo(CantDoReason::OrderAlreadyCanceled))
                ),
                "status {status} must short-circuit as already canceled"
            );
        }
    }

    #[tokio::test]
    async fn maker_cancel_of_pending_order_persists_canceled_and_notifies_bonded_taker() {
        set_global_config();
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();
        let bonded_taker = Keys::generate().public_key();

        let order = create_pending_order(maker, taker)
            .create(ctx.pool())
            .await
            .unwrap();
        insert_requested_taker_bond(ctx.pool(), order.id, &bonded_taker).await;

        let event = create_unwrapped_message_with_pubkey(maker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(result.is_ok(), "maker cancel must succeed: {result:?}");
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::Canceled.to_string()
        );
        assert!(
            queued_actions_for(maker).await.contains(&Action::Canceled),
            "maker must be notified of the cancellation"
        );
        assert!(
            queued_actions_for(bonded_taker)
                .await
                .contains(&Action::Canceled),
            "the bonded taker must be notified so they stop waiting"
        );
        let remaining = crate::app::bond::db::find_active_bonds_for_order(ctx.pool(), order.id)
            .await
            .unwrap();
        assert!(remaining.is_empty(), "the taker bond must be released");
    }

    #[tokio::test]
    async fn taker_self_cancel_with_other_active_bonds_keeps_order_parked() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();
        let rival_taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingTakerBond.to_string();
        let order = order.create(ctx.pool()).await.unwrap();
        insert_requested_taker_bond(ctx.pool(), order.id, &taker).await;
        insert_requested_taker_bond(ctx.pool(), order.id, &rival_taker).await;

        let event = create_unwrapped_message_with_pubkey(taker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(
            result.is_ok(),
            "scoped taker cancel must succeed: {result:?}"
        );
        // The rival is still racing, so the order must NOT be reset.
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::WaitingTakerBond.to_string()
        );
        let remaining = crate::app::bond::db::find_active_bonds_for_order(ctx.pool(), order.id)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1, "only the sender's bond is released");
        assert_eq!(remaining[0].pubkey, rival_taker.to_string());
        assert!(queued_actions_for(taker).await.contains(&Action::Canceled));
    }

    #[tokio::test]
    async fn taker_self_cancel_of_last_bond_resets_order_to_pending() {
        set_global_config();
        let pool = setup_pool().await;
        set_global_db_pool(&pool);
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        // Deterministic rating fallback: identity pubkey == trade pubkey.
        order.master_seller_pubkey = Some(maker.to_string());
        order.price_from_api = true;
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();
        insert_requested_taker_bond(ctx.pool(), order.id, &taker).await;

        let event = create_unwrapped_message_with_pubkey(taker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(
            result.is_ok(),
            "last-bond taker cancel must succeed: {result:?}"
        );
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.status, Status::Pending.to_string());
        assert_eq!(after.amount, 0, "api-priced amount must be reset");
        assert_eq!(after.fee, 0, "api-priced fee must be reset");
        assert!(after.hash.is_none(), "hold invoice hash must be cleared");
        assert!(
            after.buyer_pubkey.is_none(),
            "the sell-order taker side must be cleared for republish"
        );
        assert!(queued_actions_for(taker).await.contains(&Action::Canceled));
        assert!(
            queued_actions_for(maker).await.contains(&Action::NewOrder),
            "the creator must see the republished order"
        );
    }

    #[tokio::test]
    async fn pending_taker_without_bond_row_can_still_self_cancel() {
        set_global_config();
        let pool = setup_pool().await;
        set_global_db_pool(&pool);
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.master_seller_pubkey = Some(maker.to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        // No bond row: routing must fall back to the in-memory taker
        // pubkey match (buyer_pubkey == sender != creator).
        let event = create_unwrapped_message_with_pubkey(taker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(
            result.is_ok(),
            "legacy non-bond taker cancel must succeed: {result:?}"
        );
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.status, Status::Pending.to_string());
        assert!(after.buyer_pubkey.is_none());
    }

    #[tokio::test]
    async fn maker_cancel_of_waiting_payment_order_cancels_and_notifies_both() {
        set_global_config();
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingPayment.to_string();
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(maker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(result.is_ok(), "maker cancel must succeed: {result:?}");
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::Canceled.to_string()
        );
        assert!(queued_actions_for(maker).await.contains(&Action::Canceled));
        assert!(queued_actions_for(taker).await.contains(&Action::Canceled));
    }

    #[tokio::test]
    async fn taker_cancel_of_waiting_buyer_invoice_order_republishes() {
        set_global_config();
        let pool = setup_pool().await;
        set_global_db_pool(&pool);
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingBuyerInvoice.to_string();
        order.master_seller_pubkey = Some(maker.to_string());
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(taker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(result.is_ok(), "taker cancel must succeed: {result:?}");
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.status, Status::Pending.to_string());
        assert!(after.hash.is_none());
        assert!(after.buyer_pubkey.is_none());
    }

    /// A cancel racing the seller's payment must not void the escrow: the
    /// buyer is being told the payment landed at that very moment, so a refund
    /// here leaves them sending fiat against nothing.
    #[tokio::test]
    async fn maker_cancel_is_rejected_when_the_escrow_is_already_funded() {
        set_global_config();
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingPayment.to_string();
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(maker);
        let mut ln = StubEscrowLnClient::reporting(Some(InvoiceState::Accepted));
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut ln,
        )
        .await;

        assert!(
            matches!(result, Err(MostroCantDo(CantDoReason::NotAllowedByStatus))),
            "a funded escrow must reject the cancel: {result:?}"
        );
        assert!(
            !ln.escrow_was_canceled(),
            "the seller's accepted HTLC must not be refunded"
        );
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::WaitingPayment.to_string(),
            "the order must be left for the trade to advance"
        );
    }

    /// The guard sits above the maker/taker routing, so the taker side of the
    /// same race is covered too.
    #[tokio::test]
    async fn taker_cancel_is_rejected_when_the_escrow_is_already_funded() {
        set_global_config();
        let pool = setup_pool().await;
        set_global_db_pool(&pool);
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingPayment.to_string();
        order.master_seller_pubkey = Some(maker.to_string());
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(taker);
        let mut ln = StubEscrowLnClient::reporting(Some(InvoiceState::Accepted));
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut ln,
        )
        .await;

        assert!(
            matches!(result, Err(MostroCantDo(CantDoReason::NotAllowedByStatus))),
            "a funded escrow must reject the cancel: {result:?}"
        );
        assert!(!ln.escrow_was_canceled());
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::WaitingPayment.to_string()
        );
    }

    /// An unreadable escrow state is rejected rather than canceled blind, and
    /// as an internal error so the caller retries instead of believing the
    /// order is gone.
    #[tokio::test]
    async fn cancel_is_rejected_when_the_escrow_state_cannot_be_read() {
        set_global_config();
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingPayment.to_string();
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(maker);
        let mut ln = StubEscrowLnClient::unreachable();
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut ln,
        )
        .await;

        assert!(
            matches!(result, Err(MostroInternalErr(ServiceError::LnNodeError(_)))),
            "an unreadable escrow must reject the cancel: {result:?}"
        );
        assert!(!ln.escrow_was_canceled());
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::WaitingPayment.to_string()
        );
    }

    /// The buyer-side timeout still refunds the seller: in
    /// `waiting-buyer-invoice` the escrow is funded on purpose, and the guard
    /// must not stand in the way of returning it.
    #[tokio::test]
    async fn waiting_buyer_invoice_cancel_still_returns_the_funded_escrow() {
        set_global_config();
        let pool = setup_pool().await;
        set_global_db_pool(&pool);
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingBuyerInvoice.to_string();
        order.master_seller_pubkey = Some(maker.to_string());
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(taker);
        let mut ln = StubEscrowLnClient::reporting(Some(InvoiceState::Accepted));
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut ln,
        )
        .await;

        assert!(result.is_ok(), "taker cancel must succeed: {result:?}");
        assert!(
            ln.escrow_was_canceled(),
            "the seller's funds must be returned"
        );
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::Pending.to_string()
        );
    }

    /// The escrow is voided before the cancel is persisted, so a refused
    /// cancel leaves the order intact for the caller to retry. Persisting
    /// first would strand a canceled order behind a live, still-payable hold
    /// invoice that no job cleans up.
    #[tokio::test]
    async fn maker_cancel_is_not_persisted_when_the_escrow_cancel_fails() {
        set_global_config();
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingPayment.to_string();
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(maker);
        let mut ln = StubEscrowLnClient::refusing_cancel();
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut ln,
        )
        .await;

        assert!(
            matches!(result, Err(MostroInternalErr(ServiceError::LnNodeError(_)))),
            "a refused escrow cancel must surface: {result:?}"
        );
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::WaitingPayment.to_string(),
            "the order must not be canceled while its hold invoice is live"
        );
    }

    /// An escrow LND already voided must not block the cancel: that is the
    /// retry that finishes a first attempt which died between voiding the
    /// escrow and persisting, and without this it can never converge.
    #[tokio::test]
    async fn maker_cancel_converges_when_the_escrow_is_already_void() {
        set_global_config();
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingPayment.to_string();
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(maker);
        let mut ln = StubEscrowLnClient::already_canceled();
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut ln,
        )
        .await;

        assert!(
            result.is_ok(),
            "an already-void escrow must not abort the cancel: {result:?}"
        );
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::Canceled.to_string(),
            "the retry must finish what the first attempt left half-done"
        );
    }

    #[tokio::test]
    async fn cancel_not_active_order_rejects_intruder() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingPayment.to_string();
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(Keys::generate().public_key());
        let mut ln = StubEscrowLnClient::reporting(Some(InvoiceState::Accepted));
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut ln,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
        // The escrow guard runs after authorization, so a stranger neither
        // costs a lookup RPC nor learns from the error that the escrow is
        // funded — the funded escrow above would otherwise answer
        // `NotAllowedByStatus`.
        assert!(
            !ln.escrow_was_looked_up(),
            "an unauthorized sender must not reach the node"
        );
    }

    #[tokio::test]
    async fn cancel_not_active_order_with_foreign_creator_is_internal_error() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingPayment.to_string();
        // Corrupt state: the creator matches neither buyer nor seller.
        order.creator_pubkey = Keys::generate().public_key().to_string();
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(taker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
    }

    #[tokio::test]
    async fn cancel_active_order_step_1_records_buyer_as_initiator() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::Active.to_string();
        let order = order.create(ctx.pool()).await.unwrap();

        // The buyer (taker on this sell order) initiates.
        let event = create_unwrapped_message_with_pubkey(taker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(result.is_ok(), "step 1 must succeed: {result:?}");
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.cancel_initiator_pubkey, Some(taker.to_string()));
        assert!(after.buyer_cooperativecancel);
        assert!(!after.seller_cooperativecancel);
        assert!(queued_actions_for(taker)
            .await
            .contains(&Action::CooperativeCancelInitiatedByYou));
        assert!(queued_actions_for(maker)
            .await
            .contains(&Action::CooperativeCancelInitiatedByPeer));
    }

    #[tokio::test]
    async fn cancel_active_order_rejects_stranger_initiate() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::Active.to_string();
        let order = order.create(ctx.pool()).await.unwrap();

        // Neither buyer nor seller: must not be recorded as initiator.
        let stranger = Keys::generate().public_key();
        let event = create_unwrapped_message_with_pubkey(stranger);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
        let after = order_by_id(ctx.pool(), order.id).await;
        assert!(after.cancel_initiator_pubkey.is_none());
        assert!(!after.buyer_cooperativecancel);
        assert!(!after.seller_cooperativecancel);
        assert!(after.status == Status::Active.to_string());
    }

    #[tokio::test]
    async fn cancel_active_order_rejects_stranger_confirm() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::Active.to_string();
        // The buyer already initiated; escrow is still locked.
        order.cancel_initiator_pubkey = Some(taker.to_string());
        order.buyer_cooperativecancel = true;
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        // Neither buyer nor seller: must not be able to confirm the cancel.
        let stranger = Keys::generate().public_key();
        let event = create_unwrapped_message_with_pubkey(stranger);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.status, Status::Active.to_string());
        assert_eq!(after.cancel_initiator_pubkey, Some(taker.to_string()));
        assert!(after.status == Status::Active.to_string());
    }

    #[tokio::test]
    async fn cancel_fiat_sent_step_1_records_seller_as_initiator() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::FiatSent.to_string();
        let order = order.create(ctx.pool()).await.unwrap();

        // The seller (maker on this sell order) initiates.
        let event = create_unwrapped_message_with_pubkey(maker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(result.is_ok(), "step 1 must succeed: {result:?}");
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.cancel_initiator_pubkey, Some(maker.to_string()));
        assert!(after.seller_cooperativecancel);
        assert!(!after.buyer_cooperativecancel);
    }

    #[tokio::test]
    async fn cancel_active_order_step_2_rejects_same_party_confirmation() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::Active.to_string();
        order.cancel_initiator_pubkey = Some(taker.to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        // Same party (the buyer/initiator) tries to confirm its own cancel.
        let event = create_unwrapped_message_with_pubkey(taker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
    }

    #[tokio::test]
    async fn cancel_dispute_step_2_completes_cooperative_cancel_and_closes_dispute() {
        set_global_config();
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::Dispute.to_string();
        order.cancel_initiator_pubkey = Some(taker.to_string());
        order.buyer_dispute = true;
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        Dispute::new(order.id, order.status.clone())
            .create(ctx.pool())
            .await
            .unwrap();

        // The seller (maker) confirms the buyer-initiated cancel.
        let event = create_unwrapped_message_with_pubkey(maker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(result.is_ok(), "step 2 must succeed: {result:?}");
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::CooperativelyCanceled.to_string()
        );
        let dispute = crate::db::find_dispute_by_order_id(ctx.pool(), order.id)
            .await
            .unwrap();
        assert_eq!(
            dispute.status,
            DisputeStatus::SellerRefunded.to_string(),
            "the open dispute must be closed as seller-refunded"
        );
        assert!(queued_actions_for(maker)
            .await
            .contains(&Action::CooperativeCancelAccepted));
        assert!(queued_actions_for(taker)
            .await
            .contains(&Action::CooperativeCancelAccepted));
    }

    #[tokio::test]
    async fn cancel_active_order_rejects_preexisting_stranger_initiator() {
        set_global_config();
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();
        let stranger = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::Active.to_string();
        order.cancel_initiator_pubkey = Some(stranger.to_string());
        order.hash = Some("stub-hold-invoice-hash".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        // A real counterparty tries to confirm a cancel that was initiated by a stranger.
        let event = create_unwrapped_message_with_pubkey(taker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
        assert_eq!(
            order_by_id(ctx.pool(), order.id).await.status,
            Status::Active.to_string()
        );
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.cancel_initiator_pubkey, Some(stranger.to_string()));
        assert!(!after.buyer_cooperativecancel);
        assert!(!after.seller_cooperativecancel);
    }

    #[tokio::test]
    async fn cancel_action_rejects_unhandled_status() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::SettledHoldInvoice.to_string();
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(maker);
        let result = cancel_action_generic(
            &ctx,
            cancel_msg(order.id),
            &event,
            &Keys::generate(),
            &mut StubLnClient,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::NotAllowedByStatus))
        ));
    }

    #[tokio::test]
    async fn notify_creator_enqueues_new_order_and_rejects_invalid_creator() {
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let order = create_pending_order(maker, taker);
        notify_creator(&order, Some(7)).await.unwrap();
        assert!(
            queued_actions_for(maker).await.contains(&Action::NewOrder),
            "the creator must receive the republished order payload"
        );

        let mut bad_order = create_pending_order(maker, taker);
        bad_order.creator_pubkey = "not-a-valid-pubkey".to_string();
        assert!(matches!(
            notify_creator(&bad_order, None).await,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
    }
}
