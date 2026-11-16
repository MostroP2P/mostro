//! `admin-order-info`: the real state of one order, for the admin and the
//! solvers of the node (#1107).
//!
//! The public order event only says `pending`, `in-progress`, `success` or
//! `canceled`, and `orders` only answers for the caller's own orders. This
//! action answers with one snapshot of any order of the node: what the
//! database holds and, next to it, what the Lightning node reports for the
//! seller's hold invoice and for the payment to the buyer. The two are never
//! merged, because the point of the query is to see when they disagree.
//!
//! Read-only: nothing is written, published or sent to a party of the order.

use crate::app::bond::db::find_bonds_for_order_info;
use crate::app::bond::{Bond, BondRole, BondSlashReason, BondState};
use crate::app::context::AppContext;
use crate::db::{find_order_payout_marker_and_settle_time, is_solver};
use crate::lightning::{LndConnector, PaymentLookup};
use crate::util::enqueue_order_msg;
use bitcoin::hashes::hex::FromHex;
use fedimint_tonic_lnd::lnrpc::invoice::InvoiceState;
use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;
use fedimint_tonic_lnd::lnrpc::PaymentFailureReason;
use mostro_core::db::Crud;
use mostro_core::order_info as wire;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::warn;
use uuid::Uuid;

/// Bound on each Lightning lookup of a request. The event loop has a single
/// consumer and neither lookup has a timeout of its own, so without one a
/// hung node would hold every other message behind this read-only query. A
/// lookup that does not answer in time is reported as `unavailable`.
const LND_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

type LnFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, MostroError>> + Send + 'a>>;

/// The two Lightning lookups the query needs. Behind a trait (as
/// [`crate::app::cancel::CancelLightning`] and
/// [`crate::app::release::PayoutStatusLookup`] are) so every outcome can be
/// tested with a stub instead of a live node.
pub trait OrderInfoLightning {
    /// See [`LndConnector::lookup_invoice_state`].
    fn lookup_invoice_state<'a>(&'a mut self, hash: &'a str) -> LnFuture<'a, Option<InvoiceState>>;

    /// See [`LndConnector::lookup_payment`].
    fn lookup_payment<'a>(
        &'a mut self,
        payment_hash: &'a [u8],
    ) -> LnFuture<'a, Option<PaymentLookup>>;
}

impl OrderInfoLightning for LndConnector {
    fn lookup_invoice_state<'a>(&'a mut self, hash: &'a str) -> LnFuture<'a, Option<InvoiceState>> {
        Box::pin(async move { LndConnector::lookup_invoice_state(self, hash).await })
    }

    fn lookup_payment<'a>(
        &'a mut self,
        payment_hash: &'a [u8],
    ) -> LnFuture<'a, Option<PaymentLookup>> {
        Box::pin(async move { LndConnector::lookup_payment(self, payment_hash).await })
    }
}

/// Handles an `admin-order-info` request: answers the caller with an
/// [`OrderInfo`] snapshot of the order named by the message `id`.
///
/// Allowed for the Mostro admin key and for any registered solver of the
/// node (`read` or `read-write`), on any order, with or without a dispute.
/// The check runs against `event.identity`, as in the other admin actions,
/// and before the order is looked up, so a key that may not ask cannot learn
/// which order ids exist.
///
/// `ln_client` is `None` in Cashu mode, where there is no Lightning node.
///
/// # What runs where
///
/// The permission check and the database reads run here, in the event loop:
/// they are quick, and they decide every `cant-do`. The Lightning lookups do
/// not. The loop has a single consumer and drops a message that waited more
/// than 10 seconds for its turn, and a Lightning node that is slow to answer
/// is exactly when a solver asks about stuck orders. So when the order has a
/// hash to ask about, the lookups and the reply go to a task of their own,
/// with a clone of the connector, and this function returns at once. An
/// order with nothing to ask is answered inline.
///
/// # Errors
///
/// - `CantDo(InvalidPubkey)`: the caller is neither the admin nor a solver.
/// - `CantDo(NotFound)`: no order has that id.
/// - A database failure is logged and gets no reply, like in every other
///   action; the query is idempotent, so the client may ask again.
///
/// A Lightning failure is not an error: that part of the answer says
/// `unavailable` and the rest is answered as usual.
pub async fn admin_order_info_action<L>(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
    ln_client: Option<&mut L>,
) -> Result<(), MostroError>
where
    L: OrderInfoLightning + Clone + Send + 'static,
{
    answer_order_info(ctx, msg, event, my_keys, ln_client, LND_LOOKUP_TIMEOUT)
        .await
        .map(|_| ())
}

/// [`admin_order_info_action`] with the bound on each Lightning lookup as a
/// parameter, so a test does not wait the real one. Returns the task that
/// asks the Lightning node and sends the reply, when there is one; `None`
/// when the reply was queued before returning.
async fn answer_order_info<L>(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
    ln_client: Option<&mut L>,
    lookup_timeout: Duration,
) -> Result<Option<JoinHandle<()>>, MostroError>
where
    L: OrderInfoLightning + Clone + Send + 'static,
{
    let pool = ctx.pool();
    let kind = msg.get_inner_message_kind();

    if !may_ask(pool, &event.identity, my_keys).await? {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }
    let order_id = kind.id.ok_or(MostroCantDo(CantDoReason::NotFound))?;
    let stored = StoredOrder::read(pool, order_id).await?;

    // To the trade key that sent the request, where its `cant-do` would go.
    let (request_id, sender) = (kind.request_id, event.sender);
    let reply = move |info: OrderInfo| {
        enqueue_order_msg(
            request_id,
            Some(order_id),
            Action::AdminOrderInfo,
            Some(Payload::OrderInfo(info)),
            sender,
            None,
        )
    };

    // A clone of the connector, as the buyer payout takes: the event loop
    // keeps its own, and the task may outlive this dispatch.
    match ln_client.filter(|_| stored.has_lightning_part()) {
        Some(ln_client) => {
            let mut ln_client = ln_client.clone();
            Ok(Some(tokio::spawn(async move {
                let info = stored.complete(Some(&mut ln_client), lookup_timeout).await;
                reply(info).await;
            })))
        }
        None => {
            reply(stored.complete(None::<&mut L>, lookup_timeout).await).await;
            Ok(None)
        }
    }
}

/// The Mostro admin key, or a registered solver with any permission level.
async fn may_ask(
    pool: &Pool<Sqlite>,
    identity: &PublicKey,
    my_keys: &Keys,
) -> Result<bool, MostroError> {
    if *identity == my_keys.public_key() {
        return Ok(true);
    }
    is_solver(pool, &identity.to_string()).await
}

/// Builds the snapshot of one order, without sending anything.
///
/// The database is read first and in full ([`StoredOrder::read`]); the
/// Lightning node is asked after ([`StoredOrder::complete`]), and only about
/// what the row holds a hash for:
///
/// - the hold invoice, if the order has a `hash`;
/// - the payment to the buyer, if it has a `payout_payment_hash`.
///
/// Deciding by the stored hash rather than by the order status is what lets
/// the answer show a status and a Lightning state that disagree.
/// `payout_payment_hash` marks a payout in flight and is cleared when the
/// payout succeeds or fails, so after a failure the payout part is absent
/// and `failed_payment` / `payment_attempts` tell the story.
///
/// Returns `CantDo(NotFound)` when no order has that id.
pub async fn build_order_info<L>(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    ln_client: Option<&mut L>,
    lookup_timeout: Duration,
) -> Result<OrderInfo, MostroError>
where
    L: OrderInfoLightning + Send,
{
    let stored = StoredOrder::read(pool, order_id).await?;
    Ok(stored.complete(ln_client, lookup_timeout).await)
}

/// Everything the database holds for a snapshot: the half that is always
/// answered, read in one go before the Lightning node is asked anything.
struct StoredOrder {
    order: Order,
    payout_payment_hash: Option<String>,
    hold_settled_at: Option<i64>,
    bonds: Vec<BondInfo>,
}

impl StoredOrder {
    /// Reads the order, the two columns core's `Order` does not carry and
    /// the bond rows. `CantDo(NotFound)` when no order has that id.
    async fn read(pool: &Pool<Sqlite>, order_id: Uuid) -> Result<Self, MostroError> {
        let order = Order::by_id(pool, order_id)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
            .ok_or(MostroCantDo(CantDoReason::NotFound))?;
        let (payout_payment_hash, hold_settled_at) =
            find_order_payout_marker_and_settle_time(pool, order_id)
                .await?
                .unwrap_or_default();
        let bonds = find_bonds_for_order_info(pool, &order)
            .await?
            .iter()
            .map(|bond| bond_info(bond, order_id))
            .collect();
        Ok(Self {
            order,
            payout_payment_hash,
            hold_settled_at,
            bonds,
        })
    }

    fn hold_invoice_hash(&self) -> Option<&str> {
        stored(self.order.hash.as_deref())
    }

    fn payout_hash(&self) -> Option<&str> {
        stored(self.payout_payment_hash.as_deref())
    }

    /// Does the row hold a hash the Lightning node can be asked about?
    fn has_lightning_part(&self) -> bool {
        self.hold_invoice_hash().is_some() || self.payout_hash().is_some()
    }

    /// The snapshot, asking `ln_client` about each stored hash. Never
    /// fails: a lookup that cannot be made is a state of its part. Without a
    /// client a stored hash is `unavailable`.
    async fn complete<L>(self, mut ln_client: Option<&mut L>, lookup_timeout: Duration) -> OrderInfo
    where
        L: OrderInfoLightning + Send,
    {
        let hold_invoice = match self.hold_invoice_hash() {
            Some(hash) => Some(HoldInvoiceInfo {
                state: hold_invoice_state(ln_client.as_deref_mut(), hash, lookup_timeout).await,
                held_at: set_timestamp(self.order.invoice_held_at),
            }),
            None => None,
        };
        // At most one escrow part: a Cashu order has no hold invoice hash.
        let cashu_escrow = match (&hold_invoice, stored(self.order.cashu_mint_url.as_deref())) {
            (None, Some(mint_url)) => Some(CashuEscrowInfo {
                mint_url: mint_url.to_string(),
                locked_at: self.order.cashu_escrow_locked_at,
            }),
            _ => None,
        };
        let payout = match self.payout_hash() {
            Some(hash) => Some(payout_info(ln_client, hash, lookup_timeout).await),
            None => None,
        };

        let order = self.order;
        // Every field is named, with no `..Default::default()`: a field added
        // to `OrderInfo` must be decided here, not silently left empty.
        OrderInfo {
            id: order.id,
            kind: order.kind,
            status: order.status,
            amount: order.amount,
            fee: order.fee,
            routing_fee: order.routing_fee,
            fiat_code: order.fiat_code,
            fiat_amount: order.fiat_amount,
            min_amount: order.min_amount,
            max_amount: order.max_amount,
            payment_method: order.payment_method,
            premium: order.premium,
            range_parent_id: order.range_parent_id,
            created_at: order.created_at,
            taken_at: set_timestamp(order.taken_at),
            expires_at: order.expires_at,
            buyer_trade_pubkey: order.buyer_pubkey,
            seller_trade_pubkey: order.seller_pubkey,
            buyer_dispute: order.buyer_dispute,
            seller_dispute: order.seller_dispute,
            buyer_cooperativecancel: order.buyer_cooperativecancel,
            seller_cooperativecancel: order.seller_cooperativecancel,
            failed_payment: order.failed_payment,
            payment_attempts: order.payment_attempts,
            hold_settled_at: self.hold_settled_at.and_then(set_timestamp),
            hold_invoice,
            cashu_escrow,
            payout,
            bonds: self.bonds,
        }
    }
}

/// A stored text column that actually holds something.
fn stored(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// The daemon stores `0` in a timestamp that is not set yet. Sent as `null`,
/// so no client shows 1970.
fn set_timestamp(timestamp: i64) -> Option<i64> {
    (timestamp > 0).then_some(timestamp)
}

/// Asks the Lightning node for the state of the hold invoice `hash`.
async fn hold_invoice_state<L>(
    ln_client: Option<&mut L>,
    hash: &str,
    lookup_timeout: Duration,
) -> HoldInvoiceState
where
    L: OrderInfoLightning + Send,
{
    let Some(ln_client) = ln_client else {
        return HoldInvoiceState::Unavailable;
    };
    match timeout(lookup_timeout, ln_client.lookup_invoice_state(hash)).await {
        Ok(Ok(Some(InvoiceState::Open))) => HoldInvoiceState::Open,
        Ok(Ok(Some(InvoiceState::Accepted))) => HoldInvoiceState::Accepted,
        Ok(Ok(Some(InvoiceState::Settled))) => HoldInvoiceState::Settled,
        Ok(Ok(Some(InvoiceState::Canceled))) => HoldInvoiceState::Canceled,
        Ok(Ok(None)) => HoldInvoiceState::NotFound,
        Ok(Err(e)) => {
            warn!("admin-order-info: hold invoice lookup failed: {e}");
            HoldInvoiceState::Unavailable
        }
        Err(_) => {
            warn!(
                "admin-order-info: hold invoice lookup timed out after {}ms",
                lookup_timeout.as_millis()
            );
            HoldInvoiceState::Unavailable
        }
    }
}

/// Asks the Lightning node for the payment whose hash is `payment_hash`
/// (hex).
async fn payout_info<L>(
    ln_client: Option<&mut L>,
    payment_hash: &str,
    lookup_timeout: Duration,
) -> PayoutInfo
where
    L: OrderInfoLightning + Send,
{
    let state_only = |state| PayoutInfo {
        state,
        failure_reason: None,
        resolved_at: None,
    };
    let Some(ln_client) = ln_client else {
        return state_only(PayoutState::Unavailable);
    };
    let Ok(hash) = Vec::<u8>::from_hex(payment_hash) else {
        warn!("admin-order-info: stored payout payment hash is not valid hex");
        return state_only(PayoutState::Unavailable);
    };
    match timeout(lookup_timeout, ln_client.lookup_payment(&hash)).await {
        Ok(Ok(Some(found))) => payout_from_lookup(found),
        Ok(Ok(None)) => state_only(PayoutState::NotFound),
        Ok(Err(e)) => {
            warn!("admin-order-info: payout lookup failed: {e}");
            state_only(PayoutState::Unavailable)
        }
        Err(_) => {
            warn!(
                "admin-order-info: payout lookup timed out after {}ms",
                lookup_timeout.as_millis()
            );
            state_only(PayoutState::Unavailable)
        }
    }
}

/// A payment the Lightning node tracks, as the reply reports it.
fn payout_from_lookup(found: PaymentLookup) -> PayoutInfo {
    let state = match found.status {
        PaymentStatus::Succeeded => PayoutState::Succeeded,
        PaymentStatus::Failed => PayoutState::Failed,
        // `Initiated`: registered by the node, no HTLC attempted yet.
        PaymentStatus::InFlight | PaymentStatus::Initiated => PayoutState::InFlight,
        // The node tracks the payment but reports no status for it.
        PaymentStatus::Unknown => PayoutState::Unknown,
    };
    PayoutInfo {
        state,
        failure_reason: (state == PayoutState::Failed)
            .then(|| payout_failure_reason(found.failure_reason))
            .flatten(),
        resolved_at: found.resolved_at,
    }
}

/// LND's `PaymentFailureReason`, as sent on the wire, to the reply's reason.
/// `None` when the node gives none.
fn payout_failure_reason(reason: i32) -> Option<PayoutFailureReason> {
    match PaymentFailureReason::try_from(reason) {
        Ok(PaymentFailureReason::FailureReasonNone) => None,
        Ok(PaymentFailureReason::FailureReasonTimeout) => Some(PayoutFailureReason::Timeout),
        Ok(PaymentFailureReason::FailureReasonNoRoute) => Some(PayoutFailureReason::NoRoute),
        Ok(PaymentFailureReason::FailureReasonError) => Some(PayoutFailureReason::Error),
        Ok(PaymentFailureReason::FailureReasonIncorrectPaymentDetails) => {
            Some(PayoutFailureReason::IncorrectPaymentDetails)
        }
        Ok(PaymentFailureReason::FailureReasonInsufficientBalance) => {
            Some(PayoutFailureReason::InsufficientBalance)
        }
        Ok(PaymentFailureReason::FailureReasonCanceled) => Some(PayoutFailureReason::Canceled),
        // A reason this build does not map.
        _ => Some(PayoutFailureReason::Unknown),
    }
}

/// One bond row as the reply reports it. `order_id` is the requested order:
/// a row that belongs to another order is the maker bond of the range root,
/// and says so in `root_order_id`.
fn bond_info(bond: &Bond, order_id: Uuid) -> BondInfo {
    let kind = match (bond.parent_bond_id, bond.child_order_id) {
        (None, _) => BondKind::Bond,
        (Some(_), Some(_)) => BondKind::SliceSlash,
        (Some(_), None) => BondKind::MakerRefund,
    };
    let role = BondRole::from_str(&bond.role)
        .map(wire::BondRole::from)
        .unwrap_or_else(|e| {
            warn!("admin-order-info: bond {}: {e}", bond.id);
            wire::BondRole::Unknown
        });
    let state = BondState::from_str(&bond.state)
        .map(wire::BondState::from)
        .unwrap_or_else(|e| {
            warn!("admin-order-info: bond {}: {e}", bond.id);
            wire::BondState::Unknown
        });
    // A maker-refund row is not a slash: its `slashed_reason` is copied from
    // a slice row only to satisfy the payout invariant, so it is not shown.
    let slash_reason = match kind {
        BondKind::MakerRefund => None,
        _ => bond.slashed_reason.as_deref().map(|reason| {
            BondSlashReason::from_str(reason)
                .map(wire::BondSlashReason::from)
                .unwrap_or_else(|e| {
                    warn!("admin-order-info: bond {}: {e}", bond.id);
                    wire::BondSlashReason::Unknown
                })
        }),
    };
    BondInfo {
        role,
        amount: bond.amount_sats,
        state,
        slash_reason,
        kind,
        root_order_id: (bond.order_id != order_id).then_some(bond.order_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::admin_add_solver::{SOLVER_CATEGORY_READ_ONLY, SOLVER_CATEGORY_READ_WRITE};
    use crate::app::bond::db::create_bond;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::config::MESSAGE_QUEUES;
    use crate::db::add_new_user;
    use sqlx::SqlitePool;
    use std::sync::Arc;

    /// Short enough to keep the timeout tests fast.
    const TEST_TIMEOUT: Duration = Duration::from_millis(30);

    /// What a stubbed Lightning lookup does.
    #[derive(Clone, Copy)]
    enum Answer<T> {
        /// The node knows the hash.
        Found(T),
        /// The node has no record of the hash.
        Missing,
        /// The node could not be asked.
        Fails,
        /// The node never answers.
        Hangs,
    }

    impl<T> Answer<T> {
        async fn resolve(self) -> Result<Option<T>, MostroError> {
            match self {
                Answer::Found(value) => Ok(Some(value)),
                Answer::Missing => Ok(None),
                Answer::Fails => Err(MostroInternalErr(ServiceError::LnNodeError(
                    "node down".to_string(),
                ))),
                Answer::Hangs => std::future::pending().await,
            }
        }
    }

    /// A Lightning node that answers as told and records what it was asked.
    /// A clone keeps the script and its own record, like the connector the
    /// handler clones for its task.
    #[derive(Clone)]
    struct StubLn {
        invoice: Answer<InvoiceState>,
        payment: Answer<PaymentLookup>,
        invoices_asked: Vec<String>,
        payments_asked: Vec<Vec<u8>>,
    }

    impl StubLn {
        fn new(invoice: Answer<InvoiceState>, payment: Answer<PaymentLookup>) -> Self {
            Self {
                invoice,
                payment,
                invoices_asked: Vec::new(),
                payments_asked: Vec::new(),
            }
        }

        /// A node nobody should need to ask.
        fn idle() -> Self {
            Self::new(Answer::Fails, Answer::Fails)
        }
    }

    impl OrderInfoLightning for StubLn {
        fn lookup_invoice_state<'a>(
            &'a mut self,
            hash: &'a str,
        ) -> LnFuture<'a, Option<InvoiceState>> {
            self.invoices_asked.push(hash.to_string());
            Box::pin(self.invoice.resolve())
        }

        fn lookup_payment<'a>(
            &'a mut self,
            payment_hash: &'a [u8],
        ) -> LnFuture<'a, Option<PaymentLookup>> {
            self.payments_asked.push(payment_hash.to_vec());
            Box::pin(self.payment.resolve())
        }
    }

    async fn setup_ctx() -> AppContext {
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

    fn pubkey() -> String {
        Keys::generate().public_key().to_string()
    }

    fn base_order() -> Order {
        Order {
            id: Uuid::new_v4(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            status: Status::Pending.to_string(),
            creator_pubkey: pubkey(),
            fiat_code: "EUR".to_string(),
            fiat_amount: 50,
            payment_method: "SEPA".to_string(),
            amount: 100_000,
            fee: 600,
            premium: 1,
            created_at: 1_700_000_000,
            expires_at: 1_700_086_400,
            ..Default::default()
        }
    }

    async fn insert(pool: &SqlitePool, order: Order) -> Order {
        order.create(pool).await.unwrap()
    }

    /// Sets the two columns core's `Order` does not carry.
    async fn set_payout_columns(
        pool: &SqlitePool,
        order_id: Uuid,
        payout_payment_hash: Option<&str>,
        hold_settled_at: Option<i64>,
    ) {
        sqlx::query("UPDATE orders SET payout_payment_hash = ?, hold_settled_at = ? WHERE id = ?")
            .bind(payout_payment_hash)
            .bind(hold_settled_at)
            .bind(order_id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn insert_bond(
        pool: &SqlitePool,
        order_id: Uuid,
        owner: &str,
        role: BondRole,
        state: BondState,
        edit: impl FnOnce(&mut Bond),
    ) -> Bond {
        let mut bond = Bond::new_requested(order_id, owner.to_string(), role, 1_000);
        bond.state = state.to_string();
        edit(&mut bond);
        create_bond(pool, bond).await.unwrap()
    }

    async fn snapshot(ctx: &AppContext, order_id: Uuid, ln: &mut StubLn) -> OrderInfo {
        build_order_info(ctx.pool(), order_id, Some(ln), TEST_TIMEOUT)
            .await
            .unwrap()
    }

    fn request(order_id: Option<Uuid>) -> Message {
        Message::new_order(order_id, Some(7), None, Action::AdminOrderInfo, None)
    }

    fn event_from(sender: PublicKey, identity: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: request(None),
            signature: None,
            sender,
            identity,
            created_at: Timestamp::now(),
        }
    }

    /// The `admin-order-info` replies queued for `to`. The queue is
    /// process-global, so filter by destination.
    async fn replies_for(to: &PublicKey) -> Vec<Message> {
        MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(m, dest)| {
                dest == to && m.get_inner_message_kind().action == Action::AdminOrderInfo
            })
            .map(|(m, _)| m.clone())
            .collect()
    }

    async fn ask(
        ctx: &AppContext,
        mostro: &Keys,
        identity: PublicKey,
        order_id: Uuid,
    ) -> (Result<(), MostroError>, Vec<Message>) {
        let sender = Keys::generate().public_key();
        let event = event_from(sender, identity);
        let result = admin_order_info_action(
            ctx,
            request(Some(order_id)),
            &event,
            mostro,
            Some(&mut StubLn::idle()),
        )
        .await;
        (result, replies_for(&sender).await)
    }

    /// Runs the handler with `ln`, waits for its Lightning task if it
    /// started one, and returns the replies queued for the caller.
    async fn answered(
        ctx: &AppContext,
        mostro: &Keys,
        order_id: Uuid,
        ln: &mut StubLn,
        lookup_timeout: Duration,
    ) -> Vec<Message> {
        let sender = Keys::generate().public_key();
        let event = event_from(sender, mostro.public_key());
        let task = answer_order_info(
            ctx,
            request(Some(order_id)),
            &event,
            mostro,
            Some(ln),
            lookup_timeout,
        )
        .await
        .unwrap();
        if let Some(task) = task {
            task.await.unwrap();
        }
        replies_for(&sender).await
    }

    async fn register_solver(pool: &SqlitePool, category: i64) -> PublicKey {
        let solver = Keys::generate().public_key();
        add_new_user(pool, User::new(solver.to_string(), 0, 1, 0, category, 0))
            .await
            .unwrap();
        solver
    }

    // --- Who may ask -----------------------------------------------------

    #[tokio::test]
    async fn the_admin_key_gets_the_snapshot_as_a_valid_reply() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();
        let order = insert(ctx.pool(), base_order()).await;

        let (result, replies) = ask(&ctx, &mostro, mostro.public_key(), order.id).await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(replies.len(), 1);
        let reply = &replies[0];
        assert!(matches!(reply, Message::Order(_)));
        assert!(reply.verify(), "the reply must pass core's own validation");
        let kind = reply.get_inner_message_kind();
        assert_eq!(kind.id, Some(order.id));
        assert_eq!(kind.request_id, Some(7));
        match &kind.payload {
            Some(Payload::OrderInfo(info)) => {
                assert_eq!(info.id, order.id);
                assert_eq!(info.status, "pending");
                assert_eq!(info.kind, "sell");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_solver_of_either_permission_level_may_ask() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();
        // No dispute on the order: the query is not tied to one.
        let order = insert(ctx.pool(), base_order()).await;

        for category in [SOLVER_CATEGORY_READ_ONLY, SOLVER_CATEGORY_READ_WRITE] {
            let solver = register_solver(ctx.pool(), category).await;
            let (result, replies) = ask(&ctx, &mostro, solver, order.id).await;
            assert!(result.is_ok(), "category {category}: {result:?}");
            assert_eq!(replies.len(), 1, "category {category}");
        }
    }

    #[tokio::test]
    async fn anyone_else_gets_invalid_pubkey_and_no_snapshot() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();
        let mut order = base_order();
        // Not even a party of the order may ask.
        let buyer = Keys::generate().public_key();
        order.buyer_pubkey = Some(buyer.to_string());
        order.master_buyer_pubkey = Some(buyer.to_string());
        let order = insert(ctx.pool(), order).await;
        // A known user who is not a solver.
        let user = Keys::generate().public_key();
        add_new_user(ctx.pool(), User::new(user.to_string(), 0, 0, 0, 0, 0))
            .await
            .unwrap();

        for identity in [Keys::generate().public_key(), user, buyer] {
            let (result, replies) = ask(&ctx, &mostro, identity, order.id).await;
            assert!(
                matches!(result, Err(MostroCantDo(CantDoReason::InvalidPubkey))),
                "{result:?}"
            );
            assert!(replies.is_empty());
        }
    }

    #[tokio::test]
    async fn an_unknown_order_is_not_found() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();

        let (result, replies) = ask(&ctx, &mostro, mostro.public_key(), Uuid::new_v4()).await;

        assert!(
            matches!(result, Err(MostroCantDo(CantDoReason::NotFound))),
            "{result:?}"
        );
        assert!(replies.is_empty());
    }

    /// The permission check comes first, so the answer to a key that may
    /// not ask does not depend on whether the order exists.
    #[tokio::test]
    async fn a_stranger_cannot_probe_which_orders_exist() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();
        let stranger = Keys::generate().public_key();

        let (result, _) = ask(&ctx, &mostro, stranger, Uuid::new_v4()).await;

        assert!(
            matches!(result, Err(MostroCantDo(CantDoReason::InvalidPubkey))),
            "{result:?}"
        );
    }

    // --- The database part -----------------------------------------------

    #[tokio::test]
    async fn a_pending_order_has_no_lightning_parts_and_the_node_is_not_asked() {
        let ctx = setup_ctx().await;
        let order = insert(ctx.pool(), base_order()).await;
        let mut ln = StubLn::idle();

        let info = snapshot(&ctx, order.id, &mut ln).await;

        assert_eq!(info.hold_invoice, None);
        assert_eq!(info.cashu_escrow, None);
        assert_eq!(info.payout, None);
        assert!(info.bonds.is_empty());
        // Timestamps the daemon stores as 0 travel as null.
        assert_eq!(info.taken_at, None);
        assert_eq!(info.hold_settled_at, None);
        assert!(ln.invoices_asked.is_empty());
        assert!(ln.payments_asked.is_empty());
    }

    #[tokio::test]
    async fn the_order_fields_are_the_stored_ones() {
        let ctx = setup_ctx().await;
        let root = insert(ctx.pool(), base_order()).await;
        let (buyer, seller) = (pubkey(), pubkey());
        let order = insert(
            ctx.pool(),
            Order {
                status: Status::SettledHoldInvoice.to_string(),
                routing_fee: 12,
                min_amount: Some(10),
                max_amount: Some(100),
                range_parent_id: Some(root.id),
                taken_at: 1_700_000_100,
                invoice_held_at: 1_700_000_200,
                buyer_pubkey: Some(buyer.clone()),
                seller_pubkey: Some(seller.clone()),
                seller_dispute: true,
                buyer_cooperativecancel: true,
                failed_payment: true,
                payment_attempts: 2,
                ..base_order()
            },
        )
        .await;
        set_payout_columns(ctx.pool(), order.id, None, Some(1_700_000_250)).await;

        let info = snapshot(&ctx, order.id, &mut StubLn::idle()).await;

        assert_eq!(info.id, order.id);
        assert_eq!(info.kind, "sell");
        assert_eq!(info.status, "settled-hold-invoice");
        assert_eq!(info.amount, 100_000);
        assert_eq!(info.fee, 600);
        assert_eq!(info.routing_fee, 12);
        assert_eq!(info.fiat_code, "EUR");
        assert_eq!(info.fiat_amount, 50);
        assert_eq!(info.min_amount, Some(10));
        assert_eq!(info.max_amount, Some(100));
        assert_eq!(info.payment_method, "SEPA");
        assert_eq!(info.premium, 1);
        assert_eq!(info.range_parent_id, Some(root.id));
        assert_eq!(info.created_at, 1_700_000_000);
        assert_eq!(info.taken_at, Some(1_700_000_100));
        assert_eq!(info.expires_at, 1_700_086_400);
        assert_eq!(info.buyer_trade_pubkey, Some(buyer));
        assert_eq!(info.seller_trade_pubkey, Some(seller));
        assert!(!info.buyer_dispute);
        assert!(info.seller_dispute);
        assert!(info.buyer_cooperativecancel);
        assert!(!info.seller_cooperativecancel);
        // After a failed payout the marker is cleared: the payout part is
        // absent and these two database fields tell what happened.
        assert!(info.failed_payment);
        assert_eq!(info.payment_attempts, 2);
        assert_eq!(info.hold_settled_at, Some(1_700_000_250));
        assert_eq!(info.payout, None);
    }

    // --- The hold invoice ------------------------------------------------

    async fn order_with_hold_invoice(ctx: &AppContext) -> Order {
        insert(
            ctx.pool(),
            Order {
                status: Status::Active.to_string(),
                hash: Some("a1".repeat(32)),
                invoice_held_at: 1_700_000_200,
                ..base_order()
            },
        )
        .await
    }

    #[tokio::test]
    async fn the_hold_invoice_state_is_the_one_the_node_reports() {
        let ctx = setup_ctx().await;
        let order = order_with_hold_invoice(&ctx).await;

        for (reported, expected) in [
            (InvoiceState::Open, HoldInvoiceState::Open),
            (InvoiceState::Accepted, HoldInvoiceState::Accepted),
            (InvoiceState::Settled, HoldInvoiceState::Settled),
            (InvoiceState::Canceled, HoldInvoiceState::Canceled),
        ] {
            let mut ln = StubLn::new(Answer::Found(reported), Answer::Fails);
            let info = snapshot(&ctx, order.id, &mut ln).await;
            assert_eq!(
                info.hold_invoice,
                Some(HoldInvoiceInfo {
                    state: expected,
                    held_at: Some(1_700_000_200),
                })
            );
            assert_eq!(ln.invoices_asked, vec!["a1".repeat(32)]);
            // No payout marker, so the payment is not asked about.
            assert!(ln.payments_asked.is_empty());
            assert_eq!(info.payout, None);
        }
    }

    /// The status says `active` while the node says the invoice is
    /// canceled: both are reported, neither is corrected.
    #[tokio::test]
    async fn a_status_and_a_hold_invoice_that_disagree_are_both_reported() {
        let ctx = setup_ctx().await;
        let order = order_with_hold_invoice(&ctx).await;
        let mut ln = StubLn::new(Answer::Found(InvoiceState::Canceled), Answer::Fails);

        let info = snapshot(&ctx, order.id, &mut ln).await;

        assert_eq!(info.status, "active");
        assert_eq!(info.hold_invoice.unwrap().state, HoldInvoiceState::Canceled);
    }

    #[tokio::test]
    async fn a_hold_invoice_the_node_does_not_know_is_not_found() {
        let ctx = setup_ctx().await;
        let order = order_with_hold_invoice(&ctx).await;
        let mut ln = StubLn::new(Answer::Missing, Answer::Fails);

        let info = snapshot(&ctx, order.id, &mut ln).await;

        assert_eq!(info.hold_invoice.unwrap().state, HoldInvoiceState::NotFound);
    }

    /// A node that fails, or never answers, costs that part of the answer
    /// and nothing else: the database part is still there.
    #[tokio::test]
    async fn a_node_that_fails_or_hangs_leaves_the_rest_of_the_answer() {
        let ctx = setup_ctx().await;
        let order = order_with_hold_invoice(&ctx).await;
        set_payout_columns(ctx.pool(), order.id, Some(&"b2".repeat(32)), None).await;

        for (invoice, payment) in [
            (Answer::Fails, Answer::Fails),
            (Answer::Hangs, Answer::Hangs),
        ] {
            let mut ln = StubLn::new(invoice, payment);
            let info = snapshot(&ctx, order.id, &mut ln).await;

            assert_eq!(
                info.hold_invoice,
                Some(HoldInvoiceInfo {
                    state: HoldInvoiceState::Unavailable,
                    held_at: Some(1_700_000_200),
                })
            );
            assert_eq!(
                info.payout,
                Some(PayoutInfo {
                    state: PayoutState::Unavailable,
                    failure_reason: None,
                    resolved_at: None,
                })
            );
            assert_eq!(info.status, "active");
            assert_eq!(info.amount, 100_000);
        }
    }

    #[tokio::test]
    async fn a_hung_node_still_gets_the_caller_a_reply() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();
        let order = order_with_hold_invoice(&ctx).await;
        let mut ln = StubLn::new(Answer::Hangs, Answer::Hangs);

        let replies = answered(&ctx, &mostro, order.id, &mut ln, TEST_TIMEOUT).await;

        assert_eq!(replies.len(), 1);
        match &replies[0].get_inner_message_kind().payload {
            Some(Payload::OrderInfo(info)) => {
                assert_eq!(
                    info.hold_invoice.as_ref().map(|hold| hold.state),
                    Some(HoldInvoiceState::Unavailable)
                );
                assert_eq!(info.status, "active");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    /// The event loop has one consumer and drops a message that waited more
    /// than 10 seconds for its turn. A node that hangs must cost this query
    /// its Lightning part, never the other users their place in the queue:
    /// the handler returns while the lookups are still waiting.
    #[tokio::test]
    async fn a_hung_node_does_not_hold_the_event_loop() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();
        let order = order_with_hold_invoice(&ctx).await;
        set_payout_columns(ctx.pool(), order.id, Some(&"b2".repeat(32)), None).await;
        let sender = Keys::generate().public_key();
        let event = event_from(sender, mostro.public_key());
        let mut ln = StubLn::new(Answer::Hangs, Answer::Hangs);

        // A bound far longer than the test: both lookups hang for all of it.
        let returned = timeout(
            Duration::from_secs(1),
            answer_order_info(
                &ctx,
                request(Some(order.id)),
                &event,
                &mostro,
                Some(&mut ln),
                Duration::from_secs(30),
            ),
        )
        .await;

        let Ok(Ok(task)) = returned else {
            panic!("the handler waited for the Lightning node");
        };
        // The lookups are still waiting, in a task of their own: no reply
        // yet, and the caller's turn in the loop is over.
        let task = task.expect("an order with hashes is answered from a task");
        assert!(!task.is_finished());
        assert!(replies_for(&sender).await.is_empty());
        task.abort();
    }

    /// The permission check and the order lookup stay in the event loop, so
    /// a refusal is still the handler's own error, answered as a `cant-do`
    /// by the dispatch, and no task is started for it.
    #[tokio::test]
    async fn a_refusal_is_decided_before_any_task_is_started() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();
        let order = order_with_hold_invoice(&ctx).await;
        let mut ln = StubLn::new(Answer::Hangs, Answer::Hangs);

        let stranger = event_from(Keys::generate().public_key(), Keys::generate().public_key());
        let refused = answer_order_info(
            &ctx,
            request(Some(order.id)),
            &stranger,
            &mostro,
            Some(&mut ln),
            Duration::from_secs(30),
        )
        .await;
        assert!(matches!(
            refused,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));

        let admin = event_from(Keys::generate().public_key(), mostro.public_key());
        let missing = answer_order_info(
            &ctx,
            request(Some(Uuid::new_v4())),
            &admin,
            &mostro,
            Some(&mut ln),
            Duration::from_secs(30),
        )
        .await;
        assert!(matches!(missing, Err(MostroCantDo(CantDoReason::NotFound))));
    }

    /// An order with no hash has nothing to ask the node: its reply is
    /// queued before the handler returns, with no task.
    #[tokio::test]
    async fn an_order_with_nothing_to_ask_is_answered_inline() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();
        let order = insert(ctx.pool(), base_order()).await;
        let sender = Keys::generate().public_key();
        let event = event_from(sender, mostro.public_key());

        let task = answer_order_info(
            &ctx,
            request(Some(order.id)),
            &event,
            &mostro,
            Some(&mut StubLn::new(Answer::Hangs, Answer::Hangs)),
            Duration::from_secs(30),
        )
        .await
        .unwrap();

        assert!(task.is_none());
        assert_eq!(replies_for(&sender).await.len(), 1);
    }

    /// The task asks the node what the inline path would have asked, and
    /// its reply is the same snapshot `build_order_info` gives.
    #[tokio::test]
    async fn the_task_sends_the_same_snapshot() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();
        let order = order_with_hold_invoice(&ctx).await;
        set_payout_columns(ctx.pool(), order.id, Some(&"b2".repeat(32)), None).await;
        let found = lookup(
            PaymentStatus::InFlight,
            PaymentFailureReason::FailureReasonNone,
        );
        let mut ln = StubLn::new(Answer::Found(InvoiceState::Accepted), Answer::Found(found));

        let replies = answered(&ctx, &mostro, order.id, &mut ln, TEST_TIMEOUT).await;

        let expected = snapshot(&ctx, order.id, &mut ln.clone()).await;
        assert_eq!(replies.len(), 1);
        assert!(replies[0].verify());
        match &replies[0].get_inner_message_kind().payload {
            Some(Payload::OrderInfo(info)) => {
                assert_eq!(info, &expected);
                assert_eq!(
                    info.hold_invoice.as_ref().map(|hold| hold.state),
                    Some(HoldInvoiceState::Accepted)
                );
                assert_eq!(
                    info.payout.as_ref().map(|payout| payout.state),
                    Some(PayoutState::InFlight)
                );
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    // --- The payout ------------------------------------------------------

    fn lookup(status: PaymentStatus, failure_reason: PaymentFailureReason) -> PaymentLookup {
        PaymentLookup {
            status,
            resolved_at: Some(1_700_000_300),
            failure_reason: failure_reason as i32,
        }
    }

    async fn order_with_payout_in_flight(ctx: &AppContext) -> Order {
        let order = insert(
            ctx.pool(),
            Order {
                status: Status::SettledHoldInvoice.to_string(),
                ..base_order()
            },
        )
        .await;
        set_payout_columns(ctx.pool(), order.id, Some(&"b2".repeat(32)), None).await;
        order
    }

    #[tokio::test]
    async fn the_payout_state_is_the_one_the_node_reports() {
        let ctx = setup_ctx().await;
        let order = order_with_payout_in_flight(&ctx).await;
        let none = PaymentFailureReason::FailureReasonNone;

        for (status, expected) in [
            (PaymentStatus::InFlight, PayoutState::InFlight),
            (PaymentStatus::Initiated, PayoutState::InFlight),
            (PaymentStatus::Succeeded, PayoutState::Succeeded),
            (PaymentStatus::Failed, PayoutState::Failed),
            (PaymentStatus::Unknown, PayoutState::Unknown),
        ] {
            let mut ln = StubLn::new(Answer::Fails, Answer::Found(lookup(status, none)));
            let info = snapshot(&ctx, order.id, &mut ln).await;
            assert_eq!(
                info.payout,
                Some(PayoutInfo {
                    state: expected,
                    failure_reason: None,
                    resolved_at: Some(1_700_000_300),
                }),
                "{status:?}"
            );
            // Asked by the stored hash, decoded from hex.
            assert_eq!(ln.payments_asked, vec![vec![0xb2; 32]]);
            // No hold invoice hash, so the invoice is not asked about.
            assert!(ln.invoices_asked.is_empty());
            assert_eq!(info.hold_invoice, None);
        }
    }

    #[tokio::test]
    async fn a_failed_payout_carries_the_reason_the_node_gives() {
        let ctx = setup_ctx().await;
        let order = order_with_payout_in_flight(&ctx).await;

        for (reason, expected) in [
            (
                PaymentFailureReason::FailureReasonTimeout,
                PayoutFailureReason::Timeout,
            ),
            (
                PaymentFailureReason::FailureReasonNoRoute,
                PayoutFailureReason::NoRoute,
            ),
            (
                PaymentFailureReason::FailureReasonError,
                PayoutFailureReason::Error,
            ),
            (
                PaymentFailureReason::FailureReasonIncorrectPaymentDetails,
                PayoutFailureReason::IncorrectPaymentDetails,
            ),
            (
                PaymentFailureReason::FailureReasonInsufficientBalance,
                PayoutFailureReason::InsufficientBalance,
            ),
            (
                PaymentFailureReason::FailureReasonCanceled,
                PayoutFailureReason::Canceled,
            ),
        ] {
            let found = lookup(PaymentStatus::Failed, reason);
            let mut ln = StubLn::new(Answer::Fails, Answer::Found(found));
            let payout = snapshot(&ctx, order.id, &mut ln).await.payout.unwrap();
            assert_eq!(payout.state, PayoutState::Failed);
            assert_eq!(payout.failure_reason, Some(expected), "{reason:?}");
        }
    }

    #[test]
    fn a_failure_reason_this_build_does_not_map_is_unknown() {
        assert_eq!(
            payout_failure_reason(9_999),
            Some(PayoutFailureReason::Unknown)
        );
    }

    /// LND keeps the last failure reason on a payment it is retrying; a
    /// payment that did not fail has none to show.
    #[test]
    fn only_a_failed_payout_carries_a_failure_reason() {
        let found = lookup(
            PaymentStatus::InFlight,
            PaymentFailureReason::FailureReasonNoRoute,
        );
        assert_eq!(payout_from_lookup(found).failure_reason, None);
    }

    #[tokio::test]
    async fn a_payout_the_node_does_not_know_is_not_found() {
        let ctx = setup_ctx().await;
        let order = order_with_payout_in_flight(&ctx).await;
        let mut ln = StubLn::new(Answer::Fails, Answer::Missing);

        let payout = snapshot(&ctx, order.id, &mut ln).await.payout.unwrap();

        assert_eq!(payout.state, PayoutState::NotFound);
    }

    #[tokio::test]
    async fn a_payout_marker_that_is_not_a_hash_is_unavailable() {
        let ctx = setup_ctx().await;
        let order = insert(ctx.pool(), base_order()).await;
        set_payout_columns(ctx.pool(), order.id, Some("not-hex"), None).await;
        let mut ln = StubLn::idle();

        let payout = snapshot(&ctx, order.id, &mut ln).await.payout.unwrap();

        assert_eq!(payout.state, PayoutState::Unavailable);
        assert!(ln.payments_asked.is_empty());
    }

    // --- Cashu mode ------------------------------------------------------

    #[tokio::test]
    async fn a_cashu_order_reports_its_escrow_without_a_lightning_node() {
        let ctx = setup_ctx().await;
        let order = insert(
            ctx.pool(),
            Order {
                status: Status::Active.to_string(),
                cashu_mint_url: Some("https://mint.example".to_string()),
                cashu_escrow_token: Some("cashuBsecret-token".to_string()),
                cashu_escrow_locked_at: Some(1_700_000_200),
                ..base_order()
            },
        )
        .await;

        let info = build_order_info::<LndConnector>(ctx.pool(), order.id, None, TEST_TIMEOUT)
            .await
            .unwrap();

        assert_eq!(
            info.cashu_escrow,
            Some(CashuEscrowInfo {
                mint_url: "https://mint.example".to_string(),
                locked_at: Some(1_700_000_200),
            })
        );
        assert_eq!(info.hold_invoice, None);
        assert_eq!(info.payout, None);
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("cashuBsecret-token"), "{json}");
    }

    /// Without a node to ask, a stored hash is still reported: the part is
    /// `unavailable`, not absent.
    #[tokio::test]
    async fn a_stored_hash_with_no_node_to_ask_is_unavailable() {
        let ctx = setup_ctx().await;
        let order = order_with_hold_invoice(&ctx).await;
        set_payout_columns(ctx.pool(), order.id, Some(&"b2".repeat(32)), None).await;

        let info = build_order_info::<LndConnector>(ctx.pool(), order.id, None, TEST_TIMEOUT)
            .await
            .unwrap();

        assert_eq!(
            info.hold_invoice.unwrap().state,
            HoldInvoiceState::Unavailable
        );
        assert_eq!(info.payout.unwrap().state, PayoutState::Unavailable);
        assert_eq!(info.cashu_escrow, None);
    }

    // --- What it never carries -------------------------------------------

    #[tokio::test]
    async fn the_reply_carries_no_secret_of_the_order_or_its_bonds() {
        let ctx = setup_ctx().await;
        let mostro = Keys::generate();
        let (buyer, seller) = (pubkey(), pubkey());
        let secrets = [
            ("order hash", "a1".repeat(32)),
            ("order preimage", "c3".repeat(32)),
            ("buyer invoice", "lnbc1buyersecretinvoice".to_string()),
            ("master buyer pubkey", pubkey()),
            ("master seller pubkey", pubkey()),
            ("cashu token", "cashuBsecret-token".to_string()),
            ("payout payment hash", "b2".repeat(32)),
            ("bond hash", "d4".repeat(32)),
            ("bond preimage", "e5".repeat(32)),
            ("bond payment request", "lnbc1bondrequest".to_string()),
            ("bond payout invoice", "lnbc1bondpayoutinvoice".to_string()),
            ("bond payout payment hash", "f6".repeat(32)),
            ("taker identity", pubkey()),
        ];
        let secret = |name: &str| {
            let (_, value) = secrets.iter().find(|(n, _)| *n == name).unwrap();
            Some(value.clone())
        };
        let order = insert(
            ctx.pool(),
            Order {
                status: Status::SettledHoldInvoice.to_string(),
                hash: secret("order hash"),
                preimage: secret("order preimage"),
                buyer_invoice: secret("buyer invoice"),
                master_buyer_pubkey: secret("master buyer pubkey"),
                master_seller_pubkey: secret("master seller pubkey"),
                cashu_escrow_token: secret("cashu token"),
                buyer_pubkey: Some(buyer.clone()),
                seller_pubkey: Some(seller),
                ..base_order()
            },
        )
        .await;
        set_payout_columns(
            ctx.pool(),
            order.id,
            secret("payout payment hash").as_deref(),
            None,
        )
        .await;
        insert_bond(
            ctx.pool(),
            order.id,
            &buyer,
            BondRole::Taker,
            BondState::PendingPayout,
            |bond| {
                bond.hash = secret("bond hash");
                bond.preimage = secret("bond preimage");
                bond.payment_request = secret("bond payment request");
                bond.payout_invoice = secret("bond payout invoice");
                bond.payout_payment_hash = secret("bond payout payment hash");
                bond.taker_identity = secret("taker identity");
                bond.slashed_reason = Some(BondSlashReason::LostDispute.to_string());
            },
        )
        .await;

        let sender = Keys::generate().public_key();
        let event = event_from(sender, mostro.public_key());
        let mut ln = StubLn::new(
            Answer::Found(InvoiceState::Settled),
            Answer::Found(lookup(
                PaymentStatus::Failed,
                PaymentFailureReason::FailureReasonNoRoute,
            )),
        );
        let task = answer_order_info(
            &ctx,
            request(Some(order.id)),
            &event,
            &mostro,
            Some(&mut ln),
            TEST_TIMEOUT,
        )
        .await
        .unwrap();
        task.expect("the order has hashes").await.unwrap();

        let replies = replies_for(&sender).await;
        assert_eq!(replies.len(), 1);
        let json = replies[0].as_json().unwrap();
        // The reply is a full one, so there is something to leak into.
        assert!(
            json.contains(r#""hold_invoice":{"state":"settled""#),
            "{json}"
        );
        assert!(json.contains(r#""failure_reason":"no-route""#), "{json}");
        assert!(json.contains(r#""state":"pending-payout""#), "{json}");
        for (name, value) in &secrets {
            assert!(!json.contains(value), "the {name} leaked: {json}");
        }
    }

    // --- Bonds -----------------------------------------------------------

    /// What a reply says of each bond, in order.
    fn summary(info: &OrderInfo) -> Vec<(wire::BondRole, wire::BondState, BondKind)> {
        info.bonds
            .iter()
            .map(|bond| (bond.role, bond.state, bond.kind))
            .collect()
    }

    #[tokio::test]
    async fn bonds_of_the_two_parties_are_reported() {
        let ctx = setup_ctx().await;
        let (maker, taker) = (pubkey(), pubkey());
        let order = insert(
            ctx.pool(),
            Order {
                status: Status::Active.to_string(),
                creator_pubkey: maker.clone(),
                seller_pubkey: Some(maker.clone()),
                buyer_pubkey: Some(taker.clone()),
                ..base_order()
            },
        )
        .await;
        let pool = ctx.pool();
        insert_bond(
            pool,
            order.id,
            &maker,
            BondRole::Maker,
            BondState::Locked,
            |bond| {
                bond.amount_sats = 5_000;
                bond.created_at = 1;
            },
        )
        .await;
        insert_bond(
            pool,
            order.id,
            &taker,
            BondRole::Taker,
            BondState::Locked,
            |bond| bond.created_at = 2,
        )
        .await;

        let info = snapshot(&ctx, order.id, &mut StubLn::idle()).await;

        assert_eq!(
            info.bonds,
            vec![
                BondInfo {
                    role: wire::BondRole::Maker,
                    amount: 5_000,
                    state: wire::BondState::Locked,
                    slash_reason: None,
                    kind: BondKind::Bond,
                    root_order_id: None,
                },
                BondInfo {
                    role: wire::BondRole::Taker,
                    amount: 1_000,
                    state: wire::BondState::Locked,
                    slash_reason: None,
                    kind: BondKind::Bond,
                    root_order_id: None,
                },
            ]
        );
    }

    /// Bond rows are never deleted and each prospective taker gets one. Of
    /// the takers who are gone, only one still owed a payout is reported.
    #[tokio::test]
    async fn bonds_of_past_takers_are_left_out_unless_a_payout_is_owed() {
        let ctx = setup_ctx().await;
        let (maker, taker) = (pubkey(), pubkey());
        let order = insert(
            ctx.pool(),
            Order {
                status: Status::Active.to_string(),
                creator_pubkey: maker.clone(),
                seller_pubkey: Some(maker),
                buyer_pubkey: Some(taker.clone()),
                ..base_order()
            },
        )
        .await;
        let pool = ctx.pool();
        // Past takers, none of them a party of the order any more.
        let past = [
            BondState::Requested,
            BondState::Released,
            BondState::Slashed,
            BondState::Forfeited,
            BondState::PendingPayout,
            BondState::Failed,
        ];
        for (at, state) in (1..).zip(past) {
            insert_bond(pool, order.id, &pubkey(), BondRole::Taker, state, |bond| {
                bond.created_at = at;
                if state != BondState::Requested && state != BondState::Released {
                    bond.slashed_reason = Some(BondSlashReason::Timeout.to_string());
                }
            })
            .await;
        }
        // The current taker.
        insert_bond(
            pool,
            order.id,
            &taker,
            BondRole::Taker,
            BondState::Locked,
            |bond| bond.created_at = 10,
        )
        .await;
        // A bond of another order is never reported.
        let other = insert(pool, base_order()).await;
        insert_bond(
            pool,
            other.id,
            &taker,
            BondRole::Taker,
            BondState::Locked,
            |_| {},
        )
        .await;

        let info = snapshot(&ctx, order.id, &mut StubLn::idle()).await;

        assert_eq!(
            summary(&info),
            vec![
                (
                    wire::BondRole::Taker,
                    wire::BondState::PendingPayout,
                    BondKind::Bond
                ),
                (
                    wire::BondRole::Taker,
                    wire::BondState::Failed,
                    BondKind::Bond
                ),
                (
                    wire::BondRole::Taker,
                    wire::BondState::Locked,
                    BondKind::Bond
                ),
            ]
        );
        assert_eq!(
            info.bonds[0].slash_reason,
            Some(wire::BondSlashReason::Timeout)
        );
    }

    /// A taker slashed on a timeout is removed from the order, which goes
    /// back to `pending`; the maker is still owed that payout.
    #[tokio::test]
    async fn a_republished_order_still_shows_the_payout_owed_by_a_slashed_taker() {
        let ctx = setup_ctx().await;
        let maker = pubkey();
        let order = insert(
            ctx.pool(),
            Order {
                creator_pubkey: maker.clone(),
                seller_pubkey: Some(maker),
                ..base_order()
            },
        )
        .await;
        insert_bond(
            ctx.pool(),
            order.id,
            &pubkey(),
            BondRole::Taker,
            BondState::PendingPayout,
            |bond| bond.slashed_reason = Some(BondSlashReason::Timeout.to_string()),
        )
        .await;

        let info = snapshot(&ctx, order.id, &mut StubLn::idle()).await;

        assert_eq!(info.status, "pending");
        assert_eq!(info.buyer_trade_pubkey, None);
        assert_eq!(
            summary(&info),
            vec![(
                wire::BondRole::Taker,
                wire::BondState::PendingPayout,
                BondKind::Bond
            )]
        );
    }

    /// A range order with one maker bond on the root, a slash recorded
    /// against a slice and the unslashed remainder refunded on the root.
    struct RangeOrder {
        root: Order,
        slice: Order,
    }

    async fn range_order_with_a_slashed_slice(ctx: &AppContext) -> RangeOrder {
        let pool = ctx.pool();
        let maker = pubkey();
        let root = insert(
            pool,
            Order {
                creator_pubkey: maker.clone(),
                seller_pubkey: Some(maker.clone()),
                min_amount: Some(10),
                max_amount: Some(100),
                ..base_order()
            },
        )
        .await;
        let slice = insert(
            pool,
            Order {
                status: Status::CanceledByAdmin.to_string(),
                creator_pubkey: maker.clone(),
                seller_pubkey: Some(maker.clone()),
                range_parent_id: Some(root.id),
                ..base_order()
            },
        )
        .await;
        let parent = insert_bond(
            pool,
            root.id,
            &maker,
            BondRole::Maker,
            BondState::Slashed,
            |bond| {
                bond.amount_sats = 5_000;
                bond.created_at = 1;
            },
        )
        .await;
        insert_bond(
            pool,
            slice.id,
            &maker,
            BondRole::Maker,
            BondState::PendingPayout,
            |bond| {
                bond.parent_bond_id = Some(parent.id);
                bond.child_order_id = Some(slice.id);
                bond.amount_sats = 2_000;
                bond.slashed_reason = Some(BondSlashReason::LostDispute.to_string());
                bond.created_at = 2;
            },
        )
        .await;
        insert_bond(
            pool,
            root.id,
            &maker,
            BondRole::Maker,
            BondState::PendingPayout,
            |bond| {
                bond.parent_bond_id = Some(parent.id);
                bond.amount_sats = 3_000;
                // Copied from the slice row by the daemon; not a slash.
                bond.slashed_reason = Some(BondSlashReason::LostDispute.to_string());
                bond.created_at = 3;
            },
        )
        .await;
        RangeOrder { root, slice }
    }

    #[tokio::test]
    async fn a_slice_reports_its_slash_and_the_maker_bond_of_the_root() {
        let ctx = setup_ctx().await;
        let range = range_order_with_a_slashed_slice(&ctx).await;

        let info = snapshot(&ctx, range.slice.id, &mut StubLn::idle()).await;

        assert_eq!(
            info.bonds,
            vec![
                // The bond that covers the slice lives on the root order.
                BondInfo {
                    role: wire::BondRole::Maker,
                    amount: 5_000,
                    state: wire::BondState::Slashed,
                    slash_reason: None,
                    kind: BondKind::Bond,
                    root_order_id: Some(range.root.id),
                },
                BondInfo {
                    role: wire::BondRole::Maker,
                    amount: 2_000,
                    state: wire::BondState::PendingPayout,
                    slash_reason: Some(wire::BondSlashReason::LostDispute),
                    kind: BondKind::SliceSlash,
                    root_order_id: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn the_root_reports_its_maker_bond_and_the_refund() {
        let ctx = setup_ctx().await;
        let range = range_order_with_a_slashed_slice(&ctx).await;

        let info = snapshot(&ctx, range.root.id, &mut StubLn::idle()).await;

        assert_eq!(
            info.bonds,
            vec![
                BondInfo {
                    role: wire::BondRole::Maker,
                    amount: 5_000,
                    state: wire::BondState::Slashed,
                    slash_reason: None,
                    kind: BondKind::Bond,
                    root_order_id: None,
                },
                BondInfo {
                    role: wire::BondRole::Maker,
                    amount: 3_000,
                    state: wire::BondState::PendingPayout,
                    // The refund is not a slash, whatever the row holds.
                    slash_reason: None,
                    kind: BondKind::MakerRefund,
                    root_order_id: None,
                },
            ]
        );
    }

    /// A value in the `bonds` table this build cannot parse must not cost
    /// the solver the whole answer.
    #[tokio::test]
    async fn a_bond_value_this_build_does_not_know_is_reported_as_unknown() {
        let ctx = setup_ctx().await;
        let maker = pubkey();
        let order = insert(
            ctx.pool(),
            Order {
                creator_pubkey: maker.clone(),
                seller_pubkey: Some(maker.clone()),
                ..base_order()
            },
        )
        .await;
        insert_bond(
            ctx.pool(),
            order.id,
            &maker,
            BondRole::Maker,
            BondState::Locked,
            |bond| {
                bond.state = "a-state-added-later".to_string();
                bond.slashed_reason = Some("a-reason-added-later".to_string());
            },
        )
        .await;

        let info = snapshot(&ctx, order.id, &mut StubLn::idle()).await;

        assert_eq!(info.bonds.len(), 1);
        assert_eq!(info.bonds[0].role, wire::BondRole::Maker);
        assert_eq!(info.bonds[0].state, wire::BondState::Unknown);
        assert_eq!(
            info.bonds[0].slash_reason,
            Some(wire::BondSlashReason::Unknown)
        );
    }
}
