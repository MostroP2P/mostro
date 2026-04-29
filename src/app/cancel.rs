use crate::app::bond;
use crate::app::context::AppContext;
use crate::app::dispute::close_dispute_after_user_resolution;
use crate::db::{edit_pubkeys_order, update_order_to_initial_state};
use crate::lightning::LndConnector;
use crate::util::{enqueue_order_msg, get_order, update_order_event};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::info;

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

    // Phase 1: cooperative cancel always releases any taker bond. The
    // dispute slash path lands in Phase 2.
    bond::release_bonds_for_order_or_warn(pool, order.id, "cooperative_cancel").await;

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
/// - If a hold invoice exists, cancel it (refund to seller)
/// - Notify the taker
/// - Reset quote-derived amounts (if any) and return order to initial state
/// - Notify the maker/creator that the order is republished
async fn cancel_order_by_taker<L: CancelLightning + Send>(
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

    // Phase 1: the taker cancelled before activating the trade — always
    // release the bond. Slashing for timeout-based cancels is Phase 4.
    bond::release_bonds_for_order_or_warn(pool, order_updated.id, "taker_cancel").await;

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

    // Phase 1: maker cancelled before the trade went active — release any
    // taker bond that had already been locked.
    bond::release_bonds_for_order_or_warn(pool, order.id, "maker_cancel").await;

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
    if let Ok(active_bonds) =
        crate::app::bond::db::find_active_bonds_for_order(pool, order.id).await
    {
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
    bond::release_bonds_for_order_or_warn(pool, order.id, "pending_maker_cancel").await;
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

    // Pending: maker can revert to Canceled state and republish without cooperative steps.
    if order.check_status(Status::Pending).is_ok() {
        if order.sent_from_maker(event.sender).is_ok() {
            cancel_pending_order_from_maker(pool, event, &mut order, my_keys, request_id).await?;
            return Ok(());
        }
        // Phase 1: a taker who took the order but hasn't paid the bond
        // yet leaves the order in `Pending` (the taker fields are
        // populated; the bond row sits in `Requested`). Allow that taker
        // to back out — release the bond, clear the taker fields, and
        // republish the order so other takers can take it. We identify
        // the bonded taker by matching `event.sender` against an active
        // bond's `pubkey`.
        let active_bonds =
            crate::app::bond::db::find_active_bonds_for_order(pool, order.id).await?;
        let sender_str = event.sender.to_string();
        if active_bonds.iter().any(|b| b.pubkey == sender_str) {
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
    use nostr_sdk::{Keys, Timestamp};
    use sqlx::SqlitePool;
    use sqlx_crud::Crud;
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
}
