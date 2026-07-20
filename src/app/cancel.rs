use crate::app::bond;
use crate::app::context::AppContext;
use crate::app::dispute::close_dispute_after_user_resolution;
use crate::db::{edit_pubkeys_order, update_order_to_initial_state};
use crate::lightning::LndConnector;
use crate::util::{enqueue_order_msg, get_order, update_order_event};
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use std::str::FromStr;
use tracing::{info, warn};

pub trait CancelLightning {
    fn cancel_hold_invoice<'a>(
        &'a mut self,
        hash: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MostroError>> + Send + 'a>>;
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
        ln_client.cancel_hold_invoice(hash).await?;
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
        ln_client.cancel_hold_invoice(hash).await?;
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
    // We publish a new replaceable kind nostr event with the status updated
    if let Ok(order_updated) = update_order_event(my_keys, Status::Canceled, &order).await {
        order_updated
            .update(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    }
    // Cancel hold invoice if present
    if let Some(hash) = &order.hash {
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }

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
    // Publish a replaceable nostr event with updated status and persist it.
    match update_order_event(my_keys, Status::Canceled, order).await {
        Ok(order_updated) => {
            order_updated
                .update(pool)
                .await
                .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
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

    let counterparty_pubkey: String;
    if buyer_pubkey == event.sender {
        order.buyer_cooperativecancel = true;
        counterparty_pubkey = seller_pubkey.to_string();
    } else {
        order.seller_cooperativecancel = true;
        counterparty_pubkey = buyer_pubkey.to_string();
    }

    // If there is already an initiator recorded, this call becomes the confirmation (step 2).
    match order.cancel_initiator_pubkey {
        Some(_) => {
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

    if order.sent_from_maker(event.sender).is_ok() {
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
    } else if event.sender == taker_pubkey {
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
    } else {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use mostro_core::db::Crud;
    use nostr_sdk::{Keys, Timestamp};
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

    #[tokio::test]
    async fn cancel_not_active_order_rejects_intruder() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = create_pending_order(maker, taker);
        order.status = Status::WaitingPayment.to_string();
        let order = order.create(ctx.pool()).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(Keys::generate().public_key());
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
