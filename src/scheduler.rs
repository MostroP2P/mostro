use crate::app::bond;
use crate::app::context::AppContext;
use crate::app::dev_fee::run_dev_fee_cycle;
use crate::app::release::{do_payment, reconcile_inflight_payout};
use crate::config;
use crate::db::*;
use crate::escrow::EscrowBackend;
use crate::lightning::LndConnector;
use crate::price::PriceManager;
use crate::util;
use crate::LN_STATUS;
use crate::{Keys, PublicKey};

use chrono::{TimeDelta, Utc};
use config::*;
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::EventBuilder;
use nostr_sdk::prelude::{FinalizeEvent, Kind as NostrKind, Nip65Tag, Tag};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use util::{enqueue_order_msg, get_nostr_relays, send_dm, update_order_event};

pub async fn start_scheduler(ctx: AppContext) {
    info!("Creating scheduler");

    // Mode-agnostic jobs run in both Lightning and Cashu mode.
    job_expire_pending_older_orders(ctx.clone()).await;
    job_update_rate_events(ctx.clone()).await;

    // Lightning-only jobs: they settle/cancel hold invoices, retry LN
    // payments, pay the dev fee over LN, and service anti-abuse bonds — all of
    // which require an LND node that Cashu mode never initialises (CF-5).
    // Bonds are additionally mutually exclusive with Cashu mode (CF-1). Gating
    // the spawns here keeps them from calling `LndConnector::new()` on a node
    // that has no LND. Lightning mode is unaffected (`is_cashu_enabled()` is
    // `false`, so every job below still starts exactly as before).
    if !Settings::is_cashu_enabled() {
        job_cancel_orders(ctx.clone()).await;
        job_enforce_escrow_deadline(ctx.clone()).await;
        job_retry_failed_payments(ctx.clone()).await;
        job_reconcile_inflight_payouts(ctx.clone()).await;
        job_process_dev_fee_payment(ctx.clone()).await;
        job_process_bond_payouts(ctx.clone()).await;
        job_reconcile_stranded_maker_bonds(ctx.clone()).await;
    }

    // Mode-agnostic jobs (the info event self-skips when LN status is absent).
    job_orderbook_reconciler(ctx.clone()).await;
    job_info_event_send(ctx.clone()).await;
    job_relay_list(ctx.clone()).await;
    job_update_bitcoin_prices().await;
    job_flush_messages_queue(ctx.clone()).await;
    job_refresh_active_pubkeys(ctx.clone()).await;

    info!("Scheduler Started");
}

/// Periodically rebuild the protocol-v2 anti-spam gate's active-trade-pubkey
/// cache from the DB (spec §6 Phase 2). Status mutations are scattered across
/// many handlers with no single choke-point, so a periodic full reload is the
/// robust, low-coupling refresh strategy: a just-taken order's keys begin
/// fast-pathing within one `active_pubkeys_refresh_interval`. Inert on the v1
/// transport (the event loop only consults the gate for kind-14 events).
async fn job_refresh_active_pubkeys(ctx: AppContext) {
    let interval = ctx.settings().mostro.active_pubkeys_refresh_interval.max(1);
    tokio::spawn(async move {
        loop {
            match find_active_trade_pubkeys(ctx.pool()).await {
                Ok(keys) => {
                    if let Some(gate) = crate::spam_gate::SpamGate::global() {
                        let n = keys.len();
                        gate.set_known(keys);
                        tracing::debug!(
                            "spam_gate: refreshed active-trade-pubkey cache ({n} keys)"
                        );
                    }
                }
                Err(e) => {
                    warn!("spam_gate: failed to refresh active-trade-pubkey cache: {e}")
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_flush_messages_queue(ctx: AppContext) {
    // Clone for closure owning with Arc
    let order_msg_list = MESSAGE_QUEUES.queue_order_msg.clone();
    // Clone for closure owning with Arc
    let cantdo_msg_list = MESSAGE_QUEUES.queue_order_cantdo.clone();
    // Clone for closure owning with Arc
    let restore_session_msg_list = MESSAGE_QUEUES.queue_restore_session_msg.clone();
    let sender_keys = ctx.keys().clone();

    // Helper function to send messages
    async fn send_messages(
        msg_list: Arc<RwLock<Vec<(Message, PublicKey)>>>,
        sender_keys: Keys,
        retries: &mut usize,
    ) {
        if !msg_list.read().await.is_empty() {
            let (message, destination_key) = msg_list.read().await[0].clone();
            match message.as_json() {
                Ok(msg) => {
                    if let Err(e) = send_dm(destination_key, &sender_keys, &msg, None).await {
                        error!("Failed to send message: {}", e);
                        *retries += 1;
                    } else {
                        *retries = 0;
                        msg_list.write().await.remove(0);
                    }
                }
                Err(e) => error!("Failed to parse message: {}", e),
            }
            if *retries > 3 {
                *retries = 0; // Reset retries after removing message
                msg_list.write().await.remove(0);
            }
        }
    }

    // Spawn a new task to flush the messages queue
    tokio::spawn(async move {
        let mut retries_messages = 0;
        let mut retries_cantdo_messages = 0;
        let mut retries_restore_session_messages = 0;

        loop {
            send_messages(
                order_msg_list.clone(),
                sender_keys.clone(),
                &mut retries_messages,
            )
            .await;
            send_messages(
                cantdo_msg_list.clone(),
                sender_keys.clone(),
                &mut retries_cantdo_messages,
            )
            .await;
            send_messages(
                restore_session_msg_list.clone(),
                sender_keys.clone(),
                &mut retries_restore_session_messages,
            )
            .await;

            tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        }
    });
}

async fn job_relay_list(ctx: AppContext) {
    let mostro_keys = ctx.keys().clone();
    let client = ctx.nostr_client().clone();
    let interval = ctx.settings().mostro.publish_relays_interval as u64;

    tokio::spawn(async move {
        loop {
            info!("Sending Mostro relay list");
            if let Some(relays) = get_nostr_relays().await {
                let mut relay_tags: Vec<Tag> = vec![];

                for (_, r) in relays.iter() {
                    if r.status().is_connected() {
                        relay_tags.push(
                            Nip65Tag::RelayMetadata {
                                relay_url: r.url().clone(),
                                metadata: None,
                            }
                            .into(),
                        )
                    }
                }
                if let Ok(relay_ev) = EventBuilder::new(NostrKind::RelayList, "")
                    .tags(relay_tags)
                    .finalize(&mostro_keys)
                {
                    let _ = client.send_event(&relay_ev).await;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

/// Why the info-event job woke up: the regular interval elapsed, or the
/// maintenance flag changed and the `maintenance_mode` tag must go out now.
#[derive(Debug, PartialEq, Eq)]
enum InfoWake {
    Interval,
    MaintenanceChanged,
}

/// Wait for whichever comes first: the publish interval or a maintenance
/// flag change. Split out of `job_info_event_send` so the wake-up rule is
/// unit-testable without a nostr client.
async fn wait_for_info_wake(
    interval: tokio::time::Duration,
    maintenance: &crate::app::maintenance::MaintenanceState,
) -> InfoWake {
    tokio::select! {
        _ = tokio::time::sleep(interval) => InfoWake::Interval,
        _ = maintenance.changed() => InfoWake::MaintenanceChanged,
    }
}

async fn job_info_event_send(ctx: AppContext) {
    let mostro_keys = ctx.keys().clone();
    let client = ctx.nostr_client().clone();
    let maintenance = ctx.maintenance().clone();
    let interval = ctx.settings().mostro.publish_mostro_info_interval as u64;
    // The info event embeds LN node stats (`info_to_tags`). In Cashu mode there
    // is no LND, so `LN_STATUS` is never set — skip the job rather than panic
    // on `unwrap()`. A Cashu-aware info event is future work (CF-5).
    let Some(ln_status) = LN_STATUS.get() else {
        info!("Skipping mostro info event: no LN status (Cashu mode)");
        return;
    };
    tokio::spawn(async move {
        loop {
            let in_maintenance = maintenance.is_enabled();
            if in_maintenance {
                warn!("Sending info about mostro (maintenance mode ON: new orders and takes are rejected)");
            } else {
                info!("Sending info about mostro");
            }

            let tags = crate::nip33::info_to_tags(ln_status, in_maintenance);
            let id = mostro_keys.public_key().to_string();

            let info_ev = match crate::nip33::new_info_event(&mostro_keys, "", id, tags) {
                Ok(info) => info,
                Err(e) => return error!("{e}"),
            };

            let _ = client.send_event(&info_ev).await;

            if wait_for_info_wake(tokio::time::Duration::from_secs(interval), &maintenance).await
                == InfoWake::MaintenanceChanged
            {
                info!("Maintenance flag changed: republishing mostro info now");
            }
        }
    });
}

async fn job_retry_failed_payments(ctx: AppContext) {
    let ln_settings = &ctx.settings().lightning;
    let retries_number = ln_settings.payment_attempts as i64;
    let interval = ln_settings.payment_retries_interval as u64;

    tokio::spawn(async move {
        loop {
            info!(
                "I run async every {} minutes - checking for failed lighting payment",
                interval
            );

            if let Ok(payment_failed_list) = crate::db::find_failed_payment(ctx.pool()).await {
                for payment_failed in payment_failed_list.into_iter() {
                    if payment_failed.payment_attempts < retries_number {
                        if let Err(e) = do_payment(&ctx, payment_failed.clone(), None).await {
                            error!("{e}");
                        }
                    }
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

/// Floor of the payout-reconcile grace window, in seconds.
///
/// `LightningSettings::default()` has `payment_retries_interval = 0`, and a
/// 1s grace would be narrower than the claim→register window it guards,
/// re-opening the reconcile-vs-dispatch race the grace exists to close.
/// `release::PAYOUT_QUEUE_HEARTBEAT` is derived from this so a claim queued
/// for a send permit is always re-stamped before it becomes eligible for
/// reconciliation — keep that relation in mind before lowering it.
pub(crate) const MIN_GRACE_SECS: u32 = 30;

/// Reconcile buyer payouts left in flight (`payout_payment_hash` set) against
/// LND. Complements `job_retry_failed_payments`: that job only dispatches fresh
/// payouts (marker NULL), while this one resolves the durable marker so a
/// held/stranded payout — or one whose in-process watcher was lost across a
/// restart — is finalized, re-armed, or left pending based on LND's real state,
/// instead of blocking the order forever. Runs at startup and every tick.
async fn job_reconcile_inflight_payouts(ctx: AppContext) {
    // Reconcile poll cadence. Deliberately a fixed value and NOT derived from
    // `payment_retries_interval`: it only bounds how quickly a stranded payout
    // is noticed (a liveness knob), not correctness, so it stays independent of
    // the operator's retry tuning.
    const RECONCILE_INTERVAL_SECS: u64 = 60;
    // Grace window: a payout claimed less than this many seconds ago is not
    // reconciled yet, so LND has surely registered it before we ever act on a
    // `None`/`Failed` lookup. Tied to the retry cadence — comfortably larger
    // than the sub-second claim→register window.
    let grace_secs = ctx
        .settings()
        .lightning
        .payment_retries_interval
        .max(MIN_GRACE_SECS) as i64;

    tokio::spawn(async move {
        // Same capped-backoff LndConnector bootstrap as the bond payout job: a
        // transient LND outage at boot must not permanently halt reconciliation.
        let mut backoff_secs: u64 = 2;
        let mut ln_client = loop {
            match LndConnector::new().await {
                Ok(client) => break client,
                Err(e) => {
                    error!("payout reconcile: LndConnector::new failed: {e} — retrying in {backoff_secs}s");
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(60);
                }
            }
        };

        let pool = ctx.pool();
        loop {
            let claimed_before = Utc::now().timestamp() - grace_secs;
            match crate::db::find_inflight_payouts(pool, claimed_before).await {
                Ok(inflight) => {
                    for (order_id, payout_hash, payout_claimed_at) in inflight.into_iter() {
                        if let Err(e) = reconcile_inflight_payout(
                            &ctx,
                            &mut ln_client,
                            order_id,
                            &payout_hash,
                            payout_claimed_at,
                        )
                        .await
                        {
                            error!("payout reconcile for order {order_id}: {e}");
                        }
                    }
                }
                Err(e) => error!("payout reconcile: find_inflight_payouts failed: {e}"),
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(RECONCILE_INTERVAL_SECS)).await;
        }
    });
}

async fn job_update_rate_events(ctx: AppContext) {
    // Clone for closure owning with Arc
    let queue_order_rate = MESSAGE_QUEUES.queue_order_rate.clone();
    let mostro_settings = &ctx.settings().mostro;
    let interval = mostro_settings.user_rates_sent_interval_seconds as u64;
    let client = ctx.nostr_client().clone();

    tokio::spawn(async move {
        loop {
            info!(
                "I run async every {} minutes - update rate event of users",
                interval / 60
            );

            for ev in queue_order_rate.read().await.iter() {
                // Send event to relay
                let _ = client.send_event(&ev.clone()).await;
            }

            // Clear list after send events
            queue_order_rate.write().await.clear();

            let now = Utc::now();
            if let Some(next_tick) = now.checked_add_signed(
                TimeDelta::try_seconds(interval as i64).expect("Wrong seconds value"),
            ) {
                info!(
                    "Next tick for update users rating is {}",
                    next_tick.format("%a %b %e %T %Y")
                );
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

pub(crate) async fn notify_users_canceled_order(
    updated_order: &Order,
    old_order: &Order,
    maker_action: Option<Action>,
) {
    // Taker pubkey
    let taker_pubkey = if let Ok(kind) = old_order.get_order_kind() {
        match kind {
            Kind::Buy => old_order.get_seller_pubkey().map_err(MostroInternalErr),
            Kind::Sell => old_order.get_buyer_pubkey().map_err(MostroInternalErr),
        }
    } else {
        tracing::warn!("Error getting order kind in order {} cancel", old_order.id);
        return;
    };

    // get maker and taker pubkey
    let (maker_pubkey, taker_pubkey) = match (old_order.get_creator_pubkey(), taker_pubkey) {
        (Ok(maker_pubkey), Ok(taker_pubkey)) => (maker_pubkey, taker_pubkey),
        (Err(_), _) | (_, Err(_)) => {
            tracing::warn!(
                "Error getting maker and taker pubkey in order {} cancel",
                old_order.id
            );
            return;
        }
    };

    // Neutral wording on purpose: this helper serves several closure paths
    // (waiting-state timeout, hold-invoice cancel, actual cancels), so the
    // specific cause is logged by each caller, not here.
    // Both trade keys in one line links the two counterparties of a trade
    // to each other — the published order event carries no maker or taker
    // pubkey, so that pairing is not otherwise derivable. The order id is
    // the useful half and is kept.
    tracing::info!(
        "Notifying maker and taker that order {} was not completed",
        old_order.id
    );

    // get payload
    // if maker action is NewOrder, we send the order to the maker
    let (payload, maker_action) = if maker_action == Some(Action::NewOrder) {
        (
            Some(Payload::Order(SmallOrder::from(updated_order.clone()))),
            Action::NewOrder,
        )
    } else {
        (None, Action::Canceled) // if maker action is Canceled, payload is None
    };

    // notify maker that taker that the maker did not proceed with the order
    let _ = enqueue_order_msg(
        None,
        Some(updated_order.id),
        maker_action,
        payload,
        maker_pubkey,
        None,
    )
    .await;

    // notify taker that maker did not proceed with the order
    let _ = enqueue_order_msg(
        None,
        Some(updated_order.id),
        Action::Canceled,
        None,
        taker_pubkey,
        None,
    )
    .await;
}

/// Re-read a timeout candidate and confirm it is *still* eligible before
/// the scheduler acts on it.
///
/// `job_cancel_orders` selects its candidates once per tick
/// (`find_order_by_seconds`) and then works through them sequentially with
/// LND round-trips in between, so by the time an order is reached its
/// snapshot can be arbitrarily stale. Acting on the snapshot is not
/// harmless: the tick cancels the escrow hold invoice, assigns bond blame
/// from the waiting status, and cancels/republishes the order — all of
/// which would hit the wrong party or the wrong state if the order moved
/// on in the meantime (a duty handoff re-anchored `taken_at` via
/// `show_hold_invoice` / `hold_invoice_paid`, the trade activated, or a
/// cancel landed).
///
/// Returns the fresh row only when it still holds a waiting status and its
/// `taken_at` is still past the expiration window — the same predicate as
/// `find_order_by_seconds`, evaluated on current data. Anything else
/// (transitioned, re-anchored, missing, or a read error) returns `None`
/// and the caller must skip; skipping never loses a genuine timeout
/// because the next tick re-selects it.
async fn reconfirm_timeout_eligibility(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    order_id: uuid::Uuid,
    exp_seconds: u32,
) -> Option<Order> {
    let fresh = match Order::by_id(pool, order_id).await {
        Ok(Some(fresh)) => fresh,
        Ok(None) => return None,
        Err(e) => {
            warn!(
                "scheduler_timeout: could not re-read order {} to reconfirm eligibility ({e}); skipping so next tick retries",
                order_id
            );
            return None;
        }
    };
    let still_waiting = fresh.status == Status::WaitingBuyerInvoice.to_string()
        || fresh.status == Status::WaitingPayment.to_string();
    let still_expired = fresh.taken_at < Utc::now().timestamp() - exp_seconds as i64;
    (still_waiting && still_expired).then_some(fresh)
}

async fn job_cancel_orders(ctx: AppContext) {
    info!("Create a pool to connect to db");

    let keys = ctx.keys().clone();

    let mut ln_client = if let Ok(client) = LndConnector::new().await {
        client
    } else {
        return error!("Failed to create LND client");
    };
    let mostro_settings = &ctx.settings().mostro;
    let exp_seconds = mostro_settings.expiration_seconds;

    tokio::spawn(async move {
        let pool = ctx.pool();
        loop {
            info!("Check for order to republish for late actions of users");

            if let Ok(older_orders_list) = crate::db::find_order_by_seconds(pool).await {
                for order in older_orders_list.into_iter() {
                    // The tick-start snapshot may be stale by the time this
                    // iteration is reached — re-read and re-confirm before
                    // acting, so the hold-invoice cancel, the bond blame,
                    // and the cancel/republish below all run against the
                    // order's *current* duty rather than the snapshot's.
                    let Some(order) =
                        reconfirm_timeout_eligibility(pool, order.id, exp_seconds).await
                    else {
                        continue;
                    };
                    // Check if order is a sell order and Buyer is not sending the invoice for too much time.
                    // Same if seller is not paying hold invoice
                    if order.status == Status::WaitingBuyerInvoice.to_string()
                        || order.status == Status::WaitingPayment.to_string()
                    {
                        // If hold invoice is paid return funds to seller
                        // We return funds to seller
                        if let Some(hash) = order.hash.as_ref() {
                            // The cancel must succeed before we clear the
                            // order. Falling through on error would take the
                            // order out of `find_order_by_seconds`'s
                            // waiting-state eligibility window — and log
                            // "funds returned" — while the hold invoice is
                            // still encumbered, with no later tick to fix it.
                            // Same reasoning as the bond slash/release below:
                            // stay eligible and retry rather than persist a
                            // state that doesn't match the HTLC.
                            if let Err(e) = ln_client.cancel_hold_invoice(hash).await {
                                error!(
                                    "scheduler_timeout: cancel_hold_invoice failed for order {} ({e}); skipping cancel/republish so next tick retries",
                                    order.id
                                );
                                continue;
                            }
                            // A hash exists in both waiting states, but they
                            // mean different things: in `waiting-payment` the
                            // hold invoice was never paid (canceling voids
                            // it), while in `waiting-buyer-invoice` the
                            // seller already paid it and gets their funds
                            // back.
                            if order.status == Status::WaitingPayment.to_string() {
                                info!("Order Id {}: Hold invoice canceled - seller did not pay it in time", &order.id);
                            } else {
                                info!("Order Id {}: Funds returned to seller - buyer did not send their invoice in time", &order.id);
                            }
                        };
                        let mut order = order.clone();
                        // dev_fee should be reset unconditionally
                        order.dev_fee = 0;
                        // We re-publish the event with Pending status
                        // and update on local database
                        if order.price_from_api {
                            order.amount = 0;
                            order.fee = 0;
                        }

                        // Get order status and kind
                        let (order_status, order_kind) =
                            match (order.get_order_status(), order.get_order_kind()) {
                                (Ok(status), Ok(kind)) => (status, kind),
                                _ => {
                                    tracing::warn!(
                                        "Error getting order status or kind in order {} cancel",
                                        order.id
                                    );
                                    continue;
                                }
                            };

                        // Phase 4: run the bond slash/release **before** any
                        // DB mutation that takes the order out of
                        // `find_order_by_seconds`'s
                        // `status ∈ {WaitingBuyerInvoice, WaitingPayment}`
                        // eligibility window — both `update_order_to_initial_state`
                        // (republish path) and `order_updated.update`
                        // (cancel path) below are such mutations. A
                        // transient `settle_hold_invoice` failure inside
                        // `slash_one` leaves the bond `Locked`; with the
                        // slash gated on persist success (the original
                        // Phase 4 layout) that means the slash is dropped
                        // entirely, because the order has already moved
                        // out of the eligible set and the next tick never
                        // re-picks it up. Running it here means a
                        // transient LND hiccup just defers the cancel to
                        // the next tick, at which point the slash is
                        // idempotent (HTLC's "already settled" path
                        // proceeds to a CAS no-op and returns `Ok(None)`,
                        // so neither the bond nor the user are touched
                        // twice). The notification fires immediately on
                        // first success so a later persist failure
                        // doesn't lose it — by next tick `slash_or_release_on_timeout`
                        // sees no `Locked` bond and returns `Ok(None)`,
                        // so the notice never duplicates either.
                        // `order` is the pre-mutation snapshot — its
                        // waiting status and trade pubkeys are intact,
                        // which the §3.1 buyer/seller → bond mapping needs.
                        match bond::slash_or_release_on_timeout(
                            pool,
                            &mut ln_client,
                            &order,
                            Settings::get_bond(),
                        )
                        .await
                        {
                            Ok(Some(slashed)) => {
                                bond::notify_bond_slashed(&order, &slashed).await;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                // `Err` from `slash_or_release_on_timeout` is a DB-read
                                // failure (e.g. `find_active_bonds_for_order` /
                                // `timeout_slash_confirmed` couldn't read the bond
                                // rows), so we don't yet know whether the slash
                                // applies. Falling through to cancel/republish
                                // would persist the order out of
                                // `find_order_by_seconds`'s waiting-state
                                // eligibility window, and the next tick would
                                // never re-evaluate it — losing the slash whose
                                // applicability we couldn't even determine.
                                // `continue` keeps the order eligible so the
                                // next tick re-runs the full path (the slash
                                // primitive is idempotent on a settled HTLC and
                                // a `PendingPayout` bond, so a retry that
                                // finds the work already done is a no-op).
                                tracing::warn!(
                                    "scheduler_timeout: bond slash/release errored for {} ({}); skipping cancel/republish so next tick retries",
                                    order.id, e
                                );
                                continue;
                            }
                        }

                        let (maker_action, new_status, edited_order) =
                            match (order_status, order_kind) {
                                (Status::WaitingBuyerInvoice, Kind::Sell)
                                | (Status::WaitingPayment, Kind::Buy) => {
                                    // Update order status
                                    let _ = update_order_to_initial_state(
                                        pool,
                                        order.id,
                                        order.amount,
                                        order.fee,
                                        order.dev_fee,
                                    )
                                    .await;
                                    info!(
                                        "Republishing order Id {}, {}",
                                        order.id,
                                        if order_status == Status::WaitingPayment {
                                            "taker (seller) did not pay the hold invoice in time"
                                        } else {
                                            "taker (buyer) did not send their invoice in time"
                                        }
                                    );
                                    (
                                        Some(Action::NewOrder),
                                        Status::Pending,
                                        edit_pubkeys_order(pool, &order).await,
                                    )
                                }
                                (Status::WaitingBuyerInvoice, Kind::Buy)
                                | (Status::WaitingPayment, Kind::Sell) => {
                                    // Update order status
                                    info!(
                                        "Canceled order Id {}, {}",
                                        order.id,
                                        if order_status == Status::WaitingPayment {
                                            "maker (seller) did not pay the hold invoice in time"
                                        } else {
                                            "maker (buyer) did not send their invoice in time"
                                        }
                                    );
                                    (
                                        Some(Action::Canceled),
                                        Status::Canceled,
                                        edit_pubkeys_order(pool, &order).await,
                                    )
                                }
                                _ => {
                                    tracing::info!(
                                        "Order Id {} not available for cancel",
                                        &order.id
                                    );
                                    continue;
                                }
                            };

                        // Get edited order to use for update_order_event
                        let edited_order = if let Ok(edited_order) = edited_order {
                            edited_order
                        } else {
                            tracing::warn!("Error editing pubkeys in order {} cancel", order.id);
                            continue;
                        };

                        // Update order status
                        if let Ok(order_updated) =
                            update_order_event(&keys, new_status, &edited_order).await
                        {
                            // Notify users about order status changes - here order is updated
                            notify_users_canceled_order(&order_updated, &order, maker_action).await;
                            // trace new status
                            tracing::info!(
                                "Order Id {}: Reset to status {:?}",
                                &order_updated.id,
                                new_status
                            );
                            let order_id = order_updated.id;
                            // Persist the new status. The bond slash/release
                            // has already run above (before any DB mutation
                            // that strips eligibility) — on persist failure
                            // the next tick retries this branch only; the
                            // slash is durable in `bonds.slashed_reason` and
                            // a re-entry sees no `Locked` bond (or an
                            // already-recorded slice child), so it is a
                            // no-op (no duplicate notify).
                            match order_updated.update(pool).await {
                                Ok(_) => {
                                    // Phase 7: a maker-responsible timeout
                                    // cancels the order outright, terminating
                                    // its range chain — resolve the range
                                    // maker bond at close (settle + per-slice
                                    // payouts + maker refund when a slice was
                                    // slashed; plain release otherwise). The
                                    // close helper is idempotent and a cheap
                                    // no-op for non-range / already-resolved
                                    // bonds; on transient failure the
                                    // reconciliation sweep retries. The
                                    // republish branch must NOT close: the
                                    // order returns to the book with the
                                    // maker still committed.
                                    if matches!(new_status, Status::Canceled) {
                                        bond::resolve_range_maker_bond_at_close_or_warn(
                                            pool,
                                            &order,
                                            "scheduler_timeout",
                                        )
                                        .await;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "scheduler_timeout: persist failed for order {} ({}); will retry next tick",
                                        order_id, e
                                    );
                                }
                            }
                        }
                    }
                }
            }
            let now = Utc::now();
            if let Some(next_tick) = now.checked_add_signed(
                TimeDelta::try_seconds(exp_seconds as i64).expect("Wrong seconds value"),
            ) {
                info!(
                    "Next tick for late action users check is {}",
                    next_tick.format("%a %b %e %T %Y")
                );
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });
}

/// Guardian for the trade escrow's CLTV lifetime.
///
/// A trade's escrow is a hold invoice whose HTLC LND auto-cancels
/// `invoices.holdexpirydelta` blocks (default 12) before its CLTV expiry,
/// refunding the seller — while the order still reads `active` /
/// `fiat-sent` and the buyer may still send (or already sent) the fiat
/// leg. No other job covers those states, so without this pass the escrow
/// silently evaporates ~`hold_invoice_cltv_delta - 12` blocks after the
/// seller paid (~22 h on defaults) and the trade can no longer settle:
/// the buyer loses the fiat and the seller keeps both the fiat and the
/// refunded sats.
///
/// Each tick, orders whose accepted HTLC is within
/// `escrow_deadline_margin_blocks` of its CLTV expiry height — measured
/// against the actual chain tip, with the nominal 10-minute clock
/// ([`util::escrow_action_deadline_unix`]) only as a fallback when LND
/// cannot answer — are resolved while the escrow still exists:
///
/// - `active` — nobody claims the fiat moved: cancel the hold invoice
///   proactively (a clean, observed refund instead of LND's silent one),
///   move the order to `canceled`, notify both parties.
/// - `fiat-sent` — the buyer says the fiat moved: open a dispute so a
///   solver can settle or cancel *while the escrow is still settleable*.
///   The hold invoice is deliberately NOT canceled here.
async fn job_enforce_escrow_deadline(ctx: AppContext) {
    let mut ln_client = if let Ok(client) = LndConnector::new().await {
        client
    } else {
        return error!("Failed to create LND client");
    };

    tokio::spawn(async move {
        loop {
            if let Err(e) = enforce_escrow_deadline_pass(&ctx, &mut ln_client).await {
                error!("escrow deadline pass failed: {e}");
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
        }
    });
}

/// One guardian tick, split out for testing. See
/// [`job_enforce_escrow_deadline`] for the semantics.
async fn enforce_escrow_deadline_pass(
    ctx: &AppContext,
    escrow: &mut dyn EscrowBackend,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    let keys = ctx.keys().clone();
    let now = Utc::now().timestamp();
    let ln_settings = &ctx.settings().lightning;
    let cltv = ln_settings.hold_invoice_cltv_delta;
    let margin = ln_settings.escrow_deadline_margin_blocks;

    // Candidate prefilter only — real due-ness is decided from chain
    // height below. CLTV expiry is measured in blocks, not seconds, so a
    // stretch of fast blocks shortens the wall-clock window; half the
    // nominal window keeps escrows in view even at twice the 10-minute
    // average block rate.
    let cutoff = now - util::escrow_guard_window_secs(cltv, margin) / 2;
    let due_orders = find_escrow_deadline_orders(pool, cutoff).await?;

    // One probe per tick; per-order lookups below reuse it.
    let chain_height = match escrow.chain_height().await {
        Ok(height) => Some(height),
        Err(e) => {
            warn!("escrow_deadline: chain height unavailable ({e}); falling back to nominal block time");
            None
        }
    };

    for order in due_orders {
        let status = match order.get_order_status() {
            Ok(status) => status,
            Err(e) => {
                warn!(
                    "escrow_deadline: unparseable status on order {} ({e}); skipping",
                    order.id
                );
                continue;
            }
        };
        let hash = match order.hash.as_ref() {
            Some(hash) => hash.clone(),
            None => continue, // query guarantees one; defensive
        };

        // Real deadline: the accepted HTLC's expiry height vs the chain
        // tip. `due` = the guardian must act now; `escrow_gone` = the
        // HTLC verifiably no longer backs the trade (past its expiry, or
        // no accepted HTLC at LND at all).
        let heights = match chain_height {
            Some(chain) => match escrow.hold_invoice_expiry_height(&hash).await {
                Ok(Some(expiry)) => Some((chain.saturating_add(margin) >= expiry, chain >= expiry)),
                Ok(None) => Some((true, true)),
                Err(e) => {
                    warn!(
                        "escrow_deadline: HTLC expiry lookup failed for order {} ({e}); falling back to nominal block time",
                        order.id
                    );
                    None
                }
            },
            None => None,
        };
        let (due, escrow_gone) = heights.unwrap_or_else(|| {
            // Nominal-time fallback (10-minute blocks): good enough to
            // decide to act, but never proof the escrow is gone — that
            // requires chain evidence.
            (
                util::escrow_action_deadline_unix(order.invoice_held_at, cltv, margin)
                    .map(|deadline| now >= deadline)
                    .unwrap_or(false),
                false,
            )
        });
        if !due {
            continue;
        }

        match status {
            Status::Active => {
                // Cancel proactively so the seller's refund is clean and
                // observed rather than LND's silent auto-cancel. On
                // failure, stay eligible and retry next tick — unless the
                // HTLC is verifiably past its CLTV lifetime on-chain, in
                // which case LND has auto-refunded no matter what this
                // RPC reports and the state must be fixed regardless.
                if let Err(e) = escrow.cancel_hold_invoice(&hash).await {
                    match bond::flow::classify_cancel_error(&e) {
                        bond::flow::CancelOutcome::AlreadyDone => {
                            info!(
                                "escrow_deadline: order {} hold invoice already canceled at LND; fixing state",
                                order.id
                            );
                        }
                        bond::flow::CancelOutcome::Transient if !escrow_gone => {
                            warn!(
                                "escrow_deadline: cancel_hold_invoice failed for order {} ({e}); retrying next tick",
                                order.id
                            );
                            continue;
                        }
                        bond::flow::CancelOutcome::Transient => {
                            warn!(
                                "escrow_deadline: cancel_hold_invoice failed for order {} ({e}) but the HTLC is past its CLTV expiry height; the escrow is auto-refunded — fixing state",
                                order.id
                            );
                        }
                    }
                }

                let updated = match update_order_event(&keys, Status::Canceled, &order).await {
                    Ok(updated) => updated,
                    Err(e) => {
                        warn!(
                            "escrow_deadline: could not publish cancel for order {} ({e}); retrying next tick",
                            order.id
                        );
                        continue;
                    }
                };
                let updated = match updated.update(pool).await {
                    Ok(updated) => updated,
                    Err(e) => {
                        // Next tick retries: the order is still eligible, and
                        // the repeated cancel reports already-done, so the
                        // pass proceeds straight back here.
                        warn!(
                            "escrow_deadline: persist failed for order {} ({e}); retrying next tick",
                            order.id
                        );
                        continue;
                    }
                };
                info!(
                    "escrow_deadline: order {} canceled — trade escrow reached its CLTV lifetime",
                    order.id
                );
                notify_users_canceled_order(&updated, &order, Some(Action::Canceled)).await;
                // Idempotent close of any range maker bond, mirroring the
                // waiting-state timeout path.
                bond::resolve_range_maker_bond_at_close_or_warn(pool, &order, "escrow_deadline")
                    .await;
            }
            Status::FiatSent => {
                // The fiat leg may already be with the seller — only a
                // human can resolve this, and only while the escrow still
                // exists. Open (or reopen) the dispute now; do NOT cancel
                // the hold invoice, it is the solver's only leverage.
                //
                // There is at most one dispute row per order, and resolved
                // disputes keep theirs (`settled` / `seller-refunded` /
                // `released`), so an existence check is not enough: a row
                // left over from a dispute the users resolved themselves
                // earlier in the trade must be reopened, not mistaken for
                // a solver already working.
                let dispute = match find_dispute_by_order_id(pool, order.id).await {
                    Ok(dispute)
                        if matches!(
                            dispute.status.parse(),
                            Ok(DisputeStatus::Initiated | DisputeStatus::InProgress)
                        ) =>
                    {
                        // The query only returns `fiat-sent` orders, so a
                        // live dispute row here means a previous pass (or
                        // a user-filed dispute) died between writing the
                        // row and flipping the order. Resume with the
                        // existing row and finish the transition below —
                        // skipping would strand the order in `fiat-sent`
                        // with nobody notified.
                        dispute
                    }
                    Ok(mut dispute) => {
                        dispute.status = DisputeStatus::Initiated.to_string();
                        dispute.solver_pubkey = None;
                        dispute.taken_at = 0;
                        dispute.order_previous_status = order.status.clone();
                        match dispute.update(pool).await {
                            Ok(dispute) => dispute,
                            Err(e) => {
                                warn!(
                                    "escrow_deadline: could not reopen dispute for order {} ({e}); retrying next tick",
                                    order.id
                                );
                                continue;
                            }
                        }
                    }
                    Err(_) => {
                        // The buyer's claim is the one at stake, so the
                        // dispute is filed as buyer-initiated. Row first,
                        // then the status flip: a dispute row pointing at a
                        // `fiat-sent` order is recoverable (the next tick
                        // sees it and stops), a `dispute` order with no row
                        // would be invisible.
                        match Dispute::new(order.id, order.status.clone())
                            .create(pool)
                            .await
                        {
                            Ok(dispute) => dispute,
                            Err(e) => {
                                warn!(
                                    "escrow_deadline: could not create dispute for order {} ({e}); retrying next tick",
                                    order.id
                                );
                                continue;
                            }
                        }
                    }
                };
                let mut disputed = order.clone();
                if disputed.buyer_dispute {
                    // The buyer flag is already set by the earlier dispute
                    // this row was reopened from; just move the status back.
                    disputed.status = Status::Dispute.to_string();
                } else if let Err(e) = disputed.setup_dispute(true) {
                    warn!(
                        "escrow_deadline: could not move order {} to dispute ({e:?}); retrying next tick",
                        order.id
                    );
                    continue;
                }
                if let Err(e) = disputed.update(pool).await {
                    warn!(
                        "escrow_deadline: could not persist dispute status for order {} ({e}); retrying next tick",
                        order.id
                    );
                    continue;
                }
                info!(
                    "escrow_deadline: order {} moved to dispute — fiat claimed sent and the escrow is near its CLTV horizon",
                    order.id
                );
                // A missing party key must not mute the other party's
                // notification, mirroring `flow::hold_invoice_canceled`.
                for (party, action) in [
                    (order.get_buyer_pubkey(), Action::DisputeInitiatedByYou),
                    (order.get_seller_pubkey(), Action::DisputeInitiatedByPeer),
                ] {
                    match party {
                        Ok(party) => {
                            enqueue_order_msg(
                                None,
                                Some(order.id),
                                action,
                                Some(Payload::Dispute(dispute.id, None)),
                                party,
                                None,
                            )
                            .await;
                        }
                        Err(e) => warn!(
                            "escrow_deadline: could not resolve a party key for order {} ({e}); that party is not notified",
                            order.id
                        ),
                    }
                }
                // Best-effort: the dispute row is durable either way, and
                // solvers can also enumerate disputes from the admin RPC.
                if let Err(e) =
                    crate::app::dispute::publish_dispute_event(ctx, &dispute, &keys, true).await
                {
                    warn!(
                        "escrow_deadline: could not publish the dispute event for order {} ({e})",
                        order.id
                    );
                }
            }
            _ => {} // the query only returns active / fiat-sent
        }
    }

    Ok(())
}

/// How often the orderbook reconciler drains the failed-publish queue.
const ORDERBOOK_RECONCILE_INTERVAL_SECS: u64 = 60;

/// A reconciler republish only proceeds when the order has not been
/// stamped for this long. Handlers publish their kind-38383 revision
/// *before* persisting the CAS, so a background republish that reads the
/// DB row inside that window could re-advertise the pre-CAS state with a
/// newer timestamp and the relay would keep it forever. The window only
/// needs to outlast a handler's publish→persist gap (relay send timeout +
/// one DB write); anything recently stamped is retried on a later pass.
const ORDERBOOK_QUIESCENT_SECS: u64 = 120;

/// One reconciler pass: republish every order whose last kind-38383
/// publish failed (`util::take_failed_orderbook_publishes`).
///
/// Every publish goes through `update_order_event_if_quiescent`: an order
/// stamped within the last [`ORDERBOOK_QUIESCENT_SECS`] is skipped (queue
/// entries are kept for the next pass), so a republish can never supersede
/// a transition that published before persisting its CAS. On publish
/// failure the order re-queues itself, so a relay outage self-heals on a
/// later pass — up to [`util::MAX_ORDERBOOK_REPUBLISH_ATTEMPTS`] failed
/// attempts, after which the entry is dropped: a persistently-failing
/// relay cannot be converged by republishing (each retry only rewrites
/// the healthy relays' copies with a fresh `created_at`), so the cap
/// trades the retry loop for a residual divergence bounded by NIP-40.
/// The returned order (fresh `event_id`) is deliberately not persisted,
/// matching the CAS-miss repair path: the DB row's status is the source
/// of truth being re-advertised, not mutated.
async fn reconcile_orderbook_once(pool: &sqlx::SqlitePool, keys: &Keys) {
    for (order_id, generation, attempts) in util::take_failed_orderbook_publishes() {
        if attempts >= util::MAX_ORDERBOOK_REPUBLISH_ATTEMPTS {
            warn!(
                "orderbook reconciler: giving up on order {order_id} after {attempts} failed \
                 publish attempts — check the relay list; the residual divergence is bounded \
                 by the event's NIP-40 expiration"
            );
            continue;
        }
        match Order::by_id(pool, order_id).await {
            Ok(Some(order)) => match order.get_order_status() {
                Ok(status) => {
                    // Re-seed the drained entry *before* attempting the
                    // republish: the send path records its own failures via
                    // `mark_orderbook_publish_failed_at`, which must
                    // increment on top of the consumed attempts — against
                    // an absent entry it would restart the count at 1 and
                    // the attempt cap would never trip. A full success
                    // clears the entry (its stamp generation is newer), a
                    // quiescent skip leaves it untouched for the next pass.
                    util::requeue_orderbook_publish_failure(order_id, generation, attempts);
                    match util::update_order_event_if_quiescent(
                        keys,
                        status,
                        &order,
                        ORDERBOOK_QUIESCENT_SECS,
                    )
                    .await
                    {
                        Ok(Some(_)) => {}
                        // Stamped too recently — another publication may be
                        // in flight; the re-seeded entry keeps the failure
                        // queued without consuming an attempt.
                        Ok(None) => {}
                        // Pre-send failure (tags/ratings/event build):
                        // consume an attempt so a permanently broken order
                        // cannot occupy the queue forever.
                        Err(e) => {
                            warn!(
                                "orderbook reconciler: republish of order {order_id} failed: {e}"
                            );
                            util::mark_orderbook_publish_failed_at(order_id, generation);
                        }
                    }
                }
                Err(e) => warn!("orderbook reconciler: order {order_id} has bad status: {e}"),
            },
            // Row vanished — nothing to advertise; drop the queue entry.
            Ok(None) => {}
            Err(e) => {
                warn!("orderbook reconciler: could not reload order {order_id}: {e}");
                // Transient DB error: keep it queued for the next pass
                // without consuming an attempt — nothing was published.
                util::requeue_orderbook_publish_failure(order_id, generation, attempts);
            }
        }
    }
}

/// Keeps the public NIP-33 orderbook converged with the DB: drains the
/// failed-publish queue every minute. Without this job a single dropped
/// publish leaves a dead order advertised as `pending` until its NIP-40
/// expiration. Orders whose publishes succeeded are never re-sent: a
/// kind-38383 revision is published once, when the order actually
/// transitions.
///
/// Accepted trade-off: the queue is process-local and only records
/// failures the daemon observed. An entry queued right before a restart
/// is lost, and a relay that silently drops an event (or restores from a
/// backup) is never healed — periodic re-assertion of the whole book was
/// deliberately removed as unintended behavior. NIP-40 expiration bounds
/// any such divergence to the order's real take window.
async fn job_orderbook_reconciler(ctx: AppContext) {
    let keys = ctx.keys().clone();
    tokio::spawn(async move {
        let pool = ctx.pool();
        loop {
            // Sleep first: the queue is process-local, so at startup it is
            // always empty and an immediate pass could only be a no-op.
            tokio::time::sleep(tokio::time::Duration::from_secs(
                ORDERBOOK_RECONCILE_INTERVAL_SECS,
            ))
            .await;
            reconcile_orderbook_once(pool, &keys).await;
        }
    });
}

async fn job_expire_pending_older_orders(ctx: AppContext) {
    let keys = ctx.keys().clone();

    tokio::spawn(async move {
        let pool = ctx.pool();
        loop {
            info!("Check older orders and mark them Expired - check is done every minute");
            if let Ok(older_orders_list) = crate::db::find_order_by_date(pool).await {
                for order in older_orders_list.iter() {
                    tracing::info!(
                        "Order id {} - created at {} is expired",
                        order.id,
                        order.created_at
                    );

                    // Phase 5: a `WaitingMakerBond` order was never
                    // published to Nostr (the maker abandoned the bond
                    // invoice), so there is no NIP-33 event to replace.
                    // Going through `update_order_event` would publish a
                    // brand-new Expired/Canceled event for an order that
                    // never appeared in the book — a ghost entry the
                    // §10.4 acceptance forbids. Mark it Expired directly
                    // in the DB and release any bond row instead.
                    if order.status == Status::WaitingMakerBond.to_string() {
                        let order_id = order.id;
                        let mut expired = order.clone();
                        expired.status = Status::Expired.to_string();
                        match expired.update(pool).await {
                            Ok(_) => {
                                // Bonds are Lightning-only and mutually exclusive
                                // with Cashu mode (CF-1), which has no LND — the
                                // release helpers open `LndConnector::new()`, so
                                // skip them here. A cashu node should carry no
                                // bond rows; any left over (e.g. a reused DB) are
                                // a misconfiguration, not this job's concern.
                                if !Settings::is_cashu_enabled() {
                                    bond::release_bonds_for_order_or_warn(
                                        pool,
                                        order_id,
                                        "maker_bond_expiry",
                                    )
                                    .await;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "maker_bond_expiry: persist failed for order {} ({}); skipping bond release — will retry next tick",
                                    order_id, e
                                );
                            }
                        }
                        continue;
                    }

                    // We update the order id with the new event_id
                    if let Ok(order_updated) =
                        crate::util::update_order_event(&keys, Status::Expired, order).await
                    {
                        let order_id = order_updated.id;
                        // Snapshot before `update` consumes the row — the
                        // Phase 6 close hook below needs an `&Order`.
                        let order_snapshot = order_updated.clone();
                        // Same gate as the timeout job: only release
                        // bonds when the Expired status was actually
                        // persisted. On persist failure the next tick
                        // reprocesses the still-Pending order; CLTV
                        // expiry is the eventual safety net.
                        match order_updated.update(pool).await {
                            Ok(_) => {
                                // Bonds are Lightning-only and mutually exclusive
                                // with Cashu mode (CF-1); the release helpers open
                                // `LndConnector::new()`, which a cashu node has
                                // not initialised. Skip them — a cashu node
                                // carries no bond rows by construction.
                                if !Settings::is_cashu_enabled() {
                                    // Phase 1: a Pending order may be
                                    // carrying a still-active taker bond
                                    // (Phase 1 keeps the order in `Pending`
                                    // while the taker funds the bond hold
                                    // invoice). Without this hook the bond
                                    // stays in `Requested`/`Locked` and
                                    // the HTLC sits in LND until CLTV
                                    // expiry — Phase 1 promises "always
                                    // release" on every exit path,
                                    // expiry included.
                                    bond::release_taker_bonds_for_order_or_warn(
                                        pool,
                                        order_id,
                                        "pending_expiry",
                                    )
                                    .await;
                                    // Phase 6: an expiring Pending order may be a
                                    // range remainder (or the range root) — resolve
                                    // the maker bond at range close (release when no
                                    // slice was slashed; settle-at-close otherwise).
                                    // Also covers the non-range maker bond via the
                                    // close helper's non-range release branch.
                                    bond::resolve_range_maker_bond_at_close_or_warn(
                                        pool,
                                        &order_snapshot,
                                        "pending_expiry",
                                    )
                                    .await;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "pending_expiry: persist failed for order {} ({}); skipping bond release — will retry next tick",
                                    order_id, e
                                );
                            }
                        }
                    }
                }
            }
            let now = Utc::now();
            if let Some(next_tick) =
                now.checked_add_signed(TimeDelta::try_minutes(1).expect("Wrong minutes value"))
            {
                info!(
                    "Next tick for removal of older orders is {}",
                    next_tick.format("%a %b %e %T %Y")
                );
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });
}

/// Phase 6 hardening: periodically retry the settle-at-close for any range
/// maker bond left `Locked` after a terminal hook's close failed (transient
/// LND/DB error). The order's terminal-state commit is never gated on close
/// success (best-effort bond design, §8.2), so without this sweep a stranded
/// parent HTLC would sit `Locked` — blocking every slashed slice's payout —
/// until the LND CLTV safety net. The close is idempotent (CAS), so the
/// retry is safe; a parent is only touched once its whole range tree is
/// terminal, so a legitimately-open range is never disturbed. Runs every
/// 5 minutes — far below the CLTV horizon, far above any useful churn.
async fn job_reconcile_stranded_maker_bonds(ctx: AppContext) {
    let interval = 300u64;

    tokio::spawn(async move {
        let pool = ctx.pool();
        loop {
            bond::reconcile_stranded_range_maker_bonds(pool).await;
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_update_bitcoin_prices() {
    tokio::spawn(async {
        let Some(manager) = PriceManager::global() else {
            // Defensive: `main` installs the manager before the scheduler
            // is started. If that ever changes (or an embedding binary
            // skips installation) this job must not panic — every other
            // job keeps running.
            error!("price: PriceManager not installed; skipping bitcoin price job");
            return;
        };
        let configured_interval = manager.settings().update_interval_seconds;

        // Validate interval: minimum 60 seconds to avoid API rate limits.
        // Keeps the legacy guard's behaviour now that the interval moves
        // from `[mostro].exchange_rates_update_interval_seconds` to
        // `[price].update_interval_seconds` (spec §10.1).
        const MIN_INTERVAL: u64 = 60;
        let update_interval = if configured_interval < MIN_INTERVAL {
            error!(
                "price: update_interval_seconds too low: {}s (minimum: {}s). Using minimum.",
                configured_interval, MIN_INTERVAL
            );
            MIN_INTERVAL
        } else {
            configured_interval
        };

        info!(
            "Starting Bitcoin price update job (interval: {}s)",
            update_interval
        );

        loop {
            info!("Updating Bitcoin prices");
            let report = manager.update_all().await;
            // PriceManager already logs each provider's outcome per tick.
            // The scheduler only surfaces the **outage** condition — every
            // provider failed — because that's the moment ops cares about:
            // the store is now reading last-known-good across the board.
            if report.successes.is_empty() && !report.failures.is_empty() {
                let failed: Vec<String> = report
                    .failures
                    .iter()
                    .map(|(id, msg)| format!("{id}={msg}"))
                    .collect();
                error!(
                    "price: all {} providers failed this tick — serving last-known-good [{}]",
                    report.failures.len(),
                    failed.join(", ")
                );
            } else if !report.failures.is_empty() {
                // Partial outage: at least one provider failed but others
                // covered. A summary at warn is enough; per-provider info
                // is already in the manager's per-provider logs.
                warn!(
                    "price: {}/{} providers failed this tick (still {} fresh currencies)",
                    report.failures.len(),
                    report.failures.len() + report.successes.len(),
                    report.fresh_currencies
                );
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(update_interval)).await;
        }
    });
}

/// Processes unpaid development fees for completed orders.
///
/// Spawns a background task that runs [`run_dev_fee_cycle`] every 60 seconds.
/// All state-machine logic lives in [`crate::app::dev_fee`].
#[mutants::skip]
async fn job_process_dev_fee_payment(ctx: AppContext) {
    let interval = 60u64;

    let mut ln_client = if let Ok(client) = LndConnector::new().await {
        client
    } else {
        return error!("Failed to create LND client for dev fee payment job");
    };

    // On daemon restart both sets are empty so each order gets re-checked
    // once. `unverifiable` parks paid dev fees the connected node has never
    // seen (paid by a previous node after a migration) so they are not
    // re-queried every cycle (#946).
    let mut confirmed: HashSet<uuid::Uuid> = HashSet::new();
    let mut unverifiable: HashSet<uuid::Uuid> = HashSet::new();

    tokio::spawn(async move {
        let pool = ctx.pool();
        let keys = ctx.keys();
        loop {
            run_dev_fee_cycle(
                pool,
                &mut ln_client,
                &mut confirmed,
                &mut unverifiable,
                keys,
            )
            .await;
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

/// Processes bonds left in `PendingPayout` by Phase 2 / 4 / 5+.
///
/// Spawns a background task that runs
/// [`bond::run_bond_payout_cycle`] every 60 seconds, mirroring the
/// dev-fee scheduler. Not gated on `Settings::is_bond_enabled()`:
/// bonds left over from a prior enabled period must still drain when
/// an operator flips the feature off, otherwise their HTLCs sit in
/// LND with no driver. The cycle is a single indexed SELECT on
/// `bonds.state = 'pending-payout'`, which is empty for any node
/// that never enabled the feature, so the constant overhead is
/// negligible.
#[mutants::skip]
async fn job_process_bond_payouts(ctx: AppContext) {
    let interval = 60u64;

    tokio::spawn(async move {
        // Retry LndConnector::new() with capped exponential backoff so a
        // transient LND startup failure (e.g. LND not yet listening when
        // mostrod boots, or a brief restart) does not permanently halt
        // PendingPayout draining. Without this, every bond stuck in
        // PendingPayout would sit there until the operator restarts
        // mostrod — losing any chance of forfeit / payout for the
        // duration. Backoff caps at 60s to keep retry pressure modest.
        let mut backoff_secs: u64 = 2;
        let mut ln_client = loop {
            match LndConnector::new().await {
                Ok(client) => break client,
                Err(e) => {
                    error!(
                        "bond payout: LndConnector::new failed: {e} — retrying in {backoff_secs}s"
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(60);
                }
            }
        };

        let pool = ctx.pool();
        loop {
            bond::run_bond_payout_cycle(pool, &mut ln_client).await;
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::config::MOSTRO_CONFIG;
    use uuid::Uuid;

    fn init_test_settings() {
        crate::config::init_test_nostr_keys();
        let _ = MOSTRO_CONFIG.set(test_settings());
    }

    async fn migrated_ctx() -> AppContext {
        init_test_settings();
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        TestContextBuilder::new()
            .with_pool(Arc::new(pool))
            .with_settings(test_settings())
            .build()
    }

    fn hex_key() -> String {
        nostr_sdk::prelude::Keys::generate().public_key().to_hex()
    }

    fn order_for_cancel(kind: Kind, with_pubkeys: bool) -> Order {
        let (buyer, seller, creator) = if with_pubkeys {
            (Some(hex_key()), Some(hex_key()), hex_key())
        } else {
            (None, None, String::new())
        };
        Order {
            id: Uuid::new_v4(),
            kind: kind.to_string(),
            status: Status::WaitingBuyerInvoice.to_string(),
            buyer_pubkey: buyer,
            seller_pubkey: seller,
            creator_pubkey: creator,
            fiat_code: "USD".to_string(),
            payment_method: "bank".to_string(),
            ..Default::default()
        }
    }

    async fn queued_actions_for(order_id: Uuid) -> Vec<Action> {
        MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(msg, _)| msg.get_inner_message_kind().id == Some(order_id))
            .map(|(msg, _)| msg.get_inner_message_kind().action.clone())
            .collect()
    }

    // ── reconfirm_timeout_eligibility ────────────────────────────────────

    /// A waiting order whose duty clock is genuinely past the window stays
    /// eligible on the re-read: the guard must not swallow real timeouts.
    #[tokio::test]
    async fn reconfirm_timeout_eligibility_keeps_still_stale_waiting_order() {
        let ctx = migrated_ctx().await;
        let pool = ctx.pool();
        let mut order = order_for_cancel(Kind::Sell, true);
        order.taken_at = Utc::now().timestamp() - 10_000;
        let stored = order.create(pool).await.unwrap();

        let fresh = reconfirm_timeout_eligibility(pool, stored.id, 900).await;
        assert!(
            fresh.is_some(),
            "a genuinely expired waiting order must stay eligible"
        );
    }

    /// The wrongful-slash race: the tick snapshot says `waiting-buyer-invoice`
    /// with an expired clock, but before this order's turn comes up the buyer
    /// hands off — status flips to `waiting-payment` and the per-duty clock
    /// re-anchors. Acting on the snapshot would cancel the fresh escrow and
    /// blame (slash) the seller seconds into their duty.
    #[tokio::test]
    async fn reconfirm_timeout_eligibility_skips_after_duty_handoff() {
        let ctx = migrated_ctx().await;
        let pool = ctx.pool();
        let mut order = order_for_cancel(Kind::Sell, true);
        order.taken_at = Utc::now().timestamp() - 10_000;
        let stored = order.create(pool).await.unwrap();

        sqlx::query("UPDATE orders SET status = ?1, taken_at = ?2 WHERE id = ?3")
            .bind(Status::WaitingPayment.to_string())
            .bind(Utc::now().timestamp())
            .bind(stored.id)
            .execute(pool)
            .await
            .unwrap();

        assert!(
            reconfirm_timeout_eligibility(pool, stored.id, 900)
                .await
                .is_none(),
            "a just-started duty must not inherit the snapshot's expiry"
        );
    }

    /// Same status but a re-anchored clock is equally ineligible: the guard
    /// re-evaluates the `find_order_by_seconds` predicate, not just the state.
    #[tokio::test]
    async fn reconfirm_timeout_eligibility_skips_reanchored_clock() {
        let ctx = migrated_ctx().await;
        let pool = ctx.pool();
        let mut order = order_for_cancel(Kind::Sell, true);
        order.taken_at = Utc::now().timestamp() - 10_000;
        let stored = order.create(pool).await.unwrap();

        sqlx::query("UPDATE orders SET taken_at = ?1 WHERE id = ?2")
            .bind(Utc::now().timestamp())
            .bind(stored.id)
            .execute(pool)
            .await
            .unwrap();

        assert!(
            reconfirm_timeout_eligibility(pool, stored.id, 900)
                .await
                .is_none(),
            "a re-anchored clock is no longer expired"
        );
    }

    /// Orders that left the waiting window entirely — or vanished — are
    /// skipped, never acted on from the stale snapshot.
    #[tokio::test]
    async fn reconfirm_timeout_eligibility_skips_terminal_and_missing_orders() {
        let ctx = migrated_ctx().await;
        let pool = ctx.pool();
        let mut order = order_for_cancel(Kind::Sell, true);
        order.status = Status::Canceled.to_string();
        order.taken_at = Utc::now().timestamp() - 10_000;
        let stored = order.create(pool).await.unwrap();

        assert!(
            reconfirm_timeout_eligibility(pool, stored.id, 900)
                .await
                .is_none(),
            "a terminal order is not timeout-eligible however stale its clock"
        );
        assert!(
            reconfirm_timeout_eligibility(pool, Uuid::new_v4(), 900)
                .await
                .is_none(),
            "a missing row is skipped"
        );
    }

    // ── orderbook reconciler ─────────────────────────────────────────────

    /// A queue entry whose order row vanished is dropped, not retried
    /// forever: there is nothing left to advertise.
    #[tokio::test]
    async fn reconciler_drops_queue_entry_for_missing_order() {
        let ctx = migrated_ctx().await;
        let keys = ctx.keys().clone();
        let _guard = crate::util::ORDERBOOK_QUEUE_TEST_LOCK.lock().await;

        let ghost = Uuid::new_v4();
        crate::util::mark_orderbook_publish_failed(ghost);

        reconcile_orderbook_once(ctx.pool(), &keys).await;

        assert!(
            !crate::util::is_orderbook_publish_queued(ghost),
            "queue entry without a DB row must be dropped"
        );
    }

    /// While relays stay unreachable the order re-queues itself through
    /// `update_order_event`, so a later reconciler pass retries — the
    /// finding's swallowed-publish defect self-heals instead of diverging.
    #[tokio::test]
    async fn reconciler_requeues_order_while_relays_unreachable() {
        let ctx = migrated_ctx().await;
        let keys = ctx.keys().clone();
        let _guard = crate::util::ORDERBOOK_QUEUE_TEST_LOCK.lock().await;

        // A `Canceled` row still publishes a kind-38383 revision but skips
        // the reputation lookup, keeping this test off the process-global
        // DB pool.
        let order = Order {
            id: Uuid::new_v4(),
            kind: Kind::Sell.to_string(),
            status: Status::Canceled.to_string(),
            fiat_code: "USD".to_string(),
            payment_method: "bank".to_string(),
            expires_at: nostr_sdk::prelude::Timestamp::now().as_secs() as i64 + 3_600,
            ..Default::default()
        };
        let order = order.create(ctx.pool()).await.unwrap();
        crate::util::mark_orderbook_publish_failed(order.id);

        reconcile_orderbook_once(ctx.pool(), &keys).await;

        assert!(
            crate::util::is_orderbook_publish_queued(order.id),
            "unreachable relays must keep the order queued for the next pass"
        );
        // Leave the shared queue clean for other tests.
        let _ = crate::util::take_failed_orderbook_publishes();
    }

    /// An order stamped moments ago may belong to a transition that
    /// published before persisting its CAS: the reconciler must skip it
    /// this pass (keeping the queue entry) instead of re-advertising the
    /// possibly-stale DB row with a newer timestamp.
    #[tokio::test]
    async fn reconciler_skips_recently_stamped_order_but_keeps_it_queued() {
        let ctx = migrated_ctx().await;
        let keys = ctx.keys().clone();
        let _guard = crate::util::ORDERBOOK_QUEUE_TEST_LOCK.lock().await;

        let order = Order {
            id: Uuid::new_v4(),
            kind: Kind::Sell.to_string(),
            status: Status::Canceled.to_string(),
            fiat_code: "USD".to_string(),
            payment_method: "bank".to_string(),
            expires_at: nostr_sdk::prelude::Timestamp::now().as_secs() as i64 + 3_600,
            ..Default::default()
        };
        let order = order.create(ctx.pool()).await.unwrap();

        // Simulate an in-flight transition: stamp the order now, then fail
        // its publish (mark it for the reconciler).
        let _ = crate::util::stamp_orderbook_event(order.id, nostr_sdk::prelude::Timestamp::now());
        crate::util::mark_orderbook_publish_failed(order.id);

        reconcile_orderbook_once(ctx.pool(), &keys).await;

        assert!(
            crate::util::is_orderbook_publish_queued(order.id),
            "a recently stamped order must stay queued, not be republished over a possible in-flight transition"
        );
        // Leave the shared queue clean for other tests.
        let _ = crate::util::take_failed_orderbook_publishes();
    }

    /// Pins the invariant the reconciler now promises: an order whose
    /// publishes all succeeded is never re-sent. A live `pending` order
    /// that is not in the failed-publish queue must come out of a pass
    /// untouched — nothing queued and, crucially, no kind-38383 revision
    /// stamped for it.
    #[tokio::test]
    async fn reconciler_never_republishes_order_without_failed_publish() {
        let ctx = migrated_ctx().await;
        let keys = ctx.keys().clone();
        let _guard = crate::util::ORDERBOOK_QUEUE_TEST_LOCK.lock().await;

        let order = Order {
            id: Uuid::new_v4(),
            kind: Kind::Sell.to_string(),
            status: Status::Pending.to_string(),
            fiat_code: "USD".to_string(),
            payment_method: "bank".to_string(),
            expires_at: nostr_sdk::prelude::Timestamp::now().as_secs() as i64 + 3_600,
            ..Default::default()
        };
        let order = order.create(ctx.pool()).await.unwrap();

        reconcile_orderbook_once(ctx.pool(), &keys).await;

        assert!(
            !crate::util::is_orderbook_publish_queued(order.id),
            "a healthy order must not end up queued by a reconciler pass"
        );
        // If the pass had stamped (republished) the order, the monotonic
        // registry would hold an entry near `now` and bump this past
        // candidate to `last + 1`. Getting the candidate back unchanged
        // proves no stamp was created for the order during the pass.
        let probe = crate::util::stamp_orderbook_event(
            order.id,
            nostr_sdk::prelude::Timestamp::from(1_700_000_000),
        );
        assert_eq!(
            probe.created_at.as_secs(),
            1_700_000_000,
            "a reconciler pass must not stamp an order that has no failed publish"
        );
    }

    /// The drained entry is re-seeded before the republish attempt, so a
    /// failed send increments the consumed-attempt count instead of
    /// restarting it at 1 — and a quiescence skip consumes nothing.
    #[tokio::test]
    async fn reconciler_failed_retry_consumes_exactly_one_attempt() {
        let ctx = migrated_ctx().await;
        let keys = ctx.keys().clone();
        let _guard = crate::util::ORDERBOOK_QUEUE_TEST_LOCK.lock().await;

        let order = Order {
            id: Uuid::new_v4(),
            kind: Kind::Sell.to_string(),
            status: Status::Canceled.to_string(),
            fiat_code: "USD".to_string(),
            payment_method: "bank".to_string(),
            expires_at: nostr_sdk::prelude::Timestamp::now().as_secs() as i64 + 3_600,
            ..Default::default()
        };
        let order = order.create(ctx.pool()).await.unwrap();
        crate::util::mark_orderbook_publish_failed(order.id);
        assert_eq!(crate::util::orderbook_publish_attempts(order.id), Some(1));

        // No reachable relays in tests: the republish is attempted and
        // fails, which must cost exactly one more attempt.
        reconcile_orderbook_once(ctx.pool(), &keys).await;
        assert_eq!(
            crate::util::orderbook_publish_attempts(order.id),
            Some(2),
            "a failed republish must increment the consumed attempts, not reset them"
        );

        // The republish just stamped the order, so the next pass hits the
        // quiescence guard: requeued without consuming an attempt.
        reconcile_orderbook_once(ctx.pool(), &keys).await;
        assert_eq!(
            crate::util::orderbook_publish_attempts(order.id),
            Some(2),
            "a quiescence skip must not consume an attempt"
        );

        // Leave the shared queue clean for other tests.
        let _ = crate::util::take_failed_orderbook_publishes();
    }

    /// Once an entry has consumed its attempt budget the reconciler drops
    /// it instead of republishing again: the retry cannot converge a relay
    /// that keeps failing — each extra cycle would only rewrite the healthy
    /// relays' copies with a fresh `created_at`. The residual divergence is
    /// bounded by the event's NIP-40 expiration.
    #[tokio::test]
    async fn reconciler_drops_entry_after_max_attempts() {
        let ctx = migrated_ctx().await;
        let keys = ctx.keys().clone();
        let _guard = crate::util::ORDERBOOK_QUEUE_TEST_LOCK.lock().await;

        let order = Order {
            id: Uuid::new_v4(),
            kind: Kind::Sell.to_string(),
            status: Status::Canceled.to_string(),
            fiat_code: "USD".to_string(),
            payment_method: "bank".to_string(),
            expires_at: nostr_sdk::prelude::Timestamp::now().as_secs() as i64 + 3_600,
            ..Default::default()
        };
        let order = order.create(ctx.pool()).await.unwrap();
        crate::util::requeue_orderbook_publish_failure(
            order.id,
            1,
            crate::util::MAX_ORDERBOOK_REPUBLISH_ATTEMPTS,
        );

        reconcile_orderbook_once(ctx.pool(), &keys).await;

        assert!(
            !crate::util::is_orderbook_publish_queued(order.id),
            "an entry at the attempt cap must be dropped, not retried"
        );
        // No stamp may have been created for the dropped entry: probing
        // with a past candidate must return it unchanged.
        let probe = crate::util::stamp_orderbook_event(
            order.id,
            nostr_sdk::prelude::Timestamp::from(1_700_000_000),
        );
        assert_eq!(
            probe.created_at.as_secs(),
            1_700_000_000,
            "a dropped entry must not be republished"
        );
    }

    // ── notify_users_canceled_order ──────────────────────────────────────

    #[tokio::test]
    async fn notify_cancel_enqueues_republish_for_maker_and_cancel_for_taker() {
        init_test_settings();
        // Sell order: taker is the buyer.
        let order = order_for_cancel(Kind::Sell, true);
        notify_users_canceled_order(&order, &order, Some(Action::NewOrder)).await;

        let actions = queued_actions_for(order.id).await;
        assert_eq!(actions.len(), 2, "maker and taker must both be notified");
        assert!(actions.contains(&Action::NewOrder));
        assert!(actions.contains(&Action::Canceled));
    }

    #[tokio::test]
    async fn notify_cancel_enqueues_two_cancel_notices_when_order_dies() {
        init_test_settings();
        // Buy order: taker is the seller; maker action is Canceled.
        let order = order_for_cancel(Kind::Buy, true);
        notify_users_canceled_order(&order, &order, Some(Action::Canceled)).await;

        let actions = queued_actions_for(order.id).await;
        assert_eq!(actions.len(), 2);
        assert!(actions.iter().all(|a| *a == Action::Canceled));
    }

    #[tokio::test]
    async fn notify_cancel_bails_out_on_unparseable_kind() {
        init_test_settings();
        let mut order = order_for_cancel(Kind::Sell, true);
        order.kind = "swap".to_string();
        notify_users_canceled_order(&order, &order, None).await;
        assert!(queued_actions_for(order.id).await.is_empty());
    }

    #[tokio::test]
    async fn notify_cancel_bails_out_when_pubkeys_are_missing() {
        init_test_settings();
        let order = order_for_cancel(Kind::Sell, false);
        notify_users_canceled_order(&order, &order, None).await;
        assert!(queued_actions_for(order.id).await.is_empty());
    }

    // ── enforce_escrow_deadline_pass ─────────────────────────────────────

    struct StubEscrow {
        cancel_error: Option<&'static str>,
        canceled: std::sync::Mutex<Vec<String>>,
        /// `(chain tip, HTLC expiry lookup result)`. `None` = no chain
        /// view (both probes error), driving the nominal-time fallback.
        heights: Option<(u32, Option<u32>)>,
    }

    impl StubEscrow {
        fn ok() -> Self {
            Self {
                cancel_error: None,
                canceled: std::sync::Mutex::new(Vec::new()),
                heights: None,
            }
        }

        fn failing(msg: &'static str) -> Self {
            Self {
                cancel_error: Some(msg),
                canceled: std::sync::Mutex::new(Vec::new()),
                heights: None,
            }
        }

        fn with_heights(mut self, chain: u32, expiry: Option<u32>) -> Self {
            self.heights = Some((chain, expiry));
            self
        }

        fn canceled(&self) -> Vec<String> {
            self.canceled.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl EscrowBackend for StubEscrow {
        async fn create_hold_invoice(
            &mut self,
            _description: &str,
            _amount: i64,
        ) -> Result<(String, Vec<u8>, Vec<u8>), MostroError> {
            unreachable!("the guardian never creates invoices")
        }

        async fn settle_hold_invoice(&mut self, _preimage: &str) -> Result<(), MostroError> {
            unreachable!("the guardian never settles")
        }

        async fn cancel_hold_invoice(&mut self, hash: &str) -> Result<(), MostroError> {
            self.canceled.lock().unwrap().push(hash.to_string());
            match self.cancel_error {
                Some(msg) => Err(MostroInternalErr(ServiceError::LnNodeError(
                    msg.to_string(),
                ))),
                None => Ok(()),
            }
        }

        async fn chain_height(&mut self) -> Result<u32, MostroError> {
            self.heights.map(|(chain, _)| chain).ok_or_else(|| {
                MostroInternalErr(ServiceError::LnNodeError("no chain view".to_string()))
            })
        }

        async fn hold_invoice_expiry_height(
            &mut self,
            _hash: &str,
        ) -> Result<Option<u32>, MostroError> {
            self.heights.map(|(_, expiry)| expiry).ok_or_else(|| {
                MostroInternalErr(ServiceError::LnNodeError("no chain view".to_string()))
            })
        }
    }

    async fn escrow_guardian_ctx(cltv: u32, margin: u32) -> AppContext {
        init_test_settings();
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        let mut settings = test_settings();
        settings.lightning.hold_invoice_cltv_delta = cltv;
        settings.lightning.escrow_deadline_margin_blocks = margin;
        TestContextBuilder::new()
            .with_pool(Arc::new(pool))
            .with_settings(settings)
            .build()
    }

    fn escrow_backed_order(status: Status, held_at: i64) -> Order {
        Order {
            id: Uuid::new_v4(),
            kind: Kind::Sell.to_string(),
            status: status.to_string(),
            buyer_pubkey: Some(hex_key()),
            seller_pubkey: Some(hex_key()),
            creator_pubkey: hex_key(),
            fiat_code: "USD".to_string(),
            payment_method: "bank".to_string(),
            hash: Some("ab".repeat(32)),
            preimage: Some("cd".repeat(32)),
            invoice_held_at: held_at,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn escrow_deadline_cancels_due_active_order_and_refunds_seller() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        // Guard window is (144 - 24) * 600 = 72_000s; this escrow was
        // observed 80_000s ago, so it is due.
        let order = escrow_backed_order(Status::Active, now - 80_000)
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow = StubEscrow::ok();

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Canceled.to_string());
        assert_eq!(
            escrow.canceled(),
            vec![order.hash.clone().unwrap()],
            "the hold invoice must be canceled proactively, before LND does it"
        );
        let actions = queued_actions_for(order.id).await;
        assert_eq!(actions.len(), 2, "both parties must be told: {actions:?}");
        assert!(actions.iter().all(|a| *a == Action::Canceled));
    }

    #[tokio::test]
    async fn escrow_deadline_retries_when_cancel_fails_before_the_horizon() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        // Due (past the 72_000s action window) but the 86_400s hard horizon
        // has not passed: a transient LND failure must leave the order
        // eligible for the next tick instead of stranding it.
        let order = escrow_backed_order(Status::Active, now - 80_000)
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow = StubEscrow::failing("transport: connection refused");

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Active.to_string());
        assert!(queued_actions_for(order.id).await.is_empty());
    }

    #[tokio::test]
    async fn escrow_deadline_fixes_state_when_lnd_already_canceled() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        let order = escrow_backed_order(Status::Active, now - 80_000)
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow = StubEscrow::failing("invoice already canceled");

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(
            updated.status,
            Status::Canceled.to_string(),
            "the HTLC is verifiably gone — the order must not keep looking live"
        );
    }

    #[tokio::test]
    async fn escrow_deadline_opens_dispute_on_fiat_sent_without_touching_escrow() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        let order = escrow_backed_order(Status::FiatSent, now - 80_000)
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow = StubEscrow::ok();

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Dispute.to_string());
        assert!(
            find_dispute_by_order_id(ctx.pool(), order.id).await.is_ok(),
            "a solver must be able to pick up the dispute"
        );
        assert!(
            escrow.canceled().is_empty(),
            "the escrow is the solver's only leverage — never canceled here"
        );
        let actions = queued_actions_for(order.id).await;
        assert!(actions.contains(&Action::DisputeInitiatedByYou));
        assert!(actions.contains(&Action::DisputeInitiatedByPeer));
    }

    #[tokio::test]
    async fn escrow_deadline_resumes_a_half_completed_dispute_transition() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        let order = escrow_backed_order(Status::FiatSent, now - 80_000)
            .create(ctx.pool())
            .await
            .unwrap();
        // An `initiated` dispute row on a still-`fiat-sent` order can only
        // mean a previous pass (or a user dispute) died between the two
        // writes: the pass must finish the transition, not skip it.
        let prior = Dispute::new(order.id, order.status.clone())
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow = StubEscrow::ok();

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Dispute.to_string());
        let dispute = find_dispute_by_order_id(ctx.pool(), order.id)
            .await
            .unwrap();
        assert_eq!(dispute.id, prior.id, "the existing row is reused");
        assert_eq!(dispute.status, DisputeStatus::Initiated.to_string());
        assert!(escrow.canceled().is_empty());
        let actions = queued_actions_for(order.id).await;
        assert!(actions.contains(&Action::DisputeInitiatedByYou));
        assert!(actions.contains(&Action::DisputeInitiatedByPeer));
    }

    #[tokio::test]
    async fn escrow_deadline_reopens_a_dispute_the_users_resolved_themselves() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        let order = escrow_backed_order(Status::FiatSent, now - 80_000)
            .create(ctx.pool())
            .await
            .unwrap();
        // A dispute the users resolved cooperatively earlier in the trade:
        // its row lingers with a terminal status and must not masquerade
        // as a solver already working on the escrow deadline.
        let mut prior = Dispute::new(order.id, order.status.clone());
        prior.status = DisputeStatus::Settled.to_string();
        prior.solver_pubkey = Some(hex_key());
        prior.taken_at = now - 90_000;
        let prior = prior.create(ctx.pool()).await.unwrap();
        let mut escrow = StubEscrow::ok();

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Dispute.to_string());
        let dispute = find_dispute_by_order_id(ctx.pool(), order.id)
            .await
            .unwrap();
        assert_eq!(dispute.id, prior.id, "one dispute row per order, reopened");
        assert_eq!(dispute.status, DisputeStatus::Initiated.to_string());
        assert_eq!(dispute.solver_pubkey, None);
        assert_eq!(dispute.taken_at, 0);
        assert!(escrow.canceled().is_empty());
        let actions = queued_actions_for(order.id).await;
        assert!(actions.contains(&Action::DisputeInitiatedByYou));
        assert!(actions.contains(&Action::DisputeInitiatedByPeer));
    }

    #[tokio::test]
    async fn escrow_deadline_acts_on_chain_height_when_blocks_run_fast() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        // Only 40_000s elapsed — inside the nominal 72_000s window — but
        // the chain says the HTLC is within the 24-block margin of its
        // expiry: blocks came in fast, and the guardian must act now.
        let order = escrow_backed_order(Status::Active, now - 40_000)
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow = StubEscrow::ok().with_heights(900_000, Some(900_010));

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Canceled.to_string());
        assert_eq!(escrow.canceled(), vec![order.hash.clone().unwrap()]);
    }

    #[tokio::test]
    async fn escrow_deadline_trusts_a_live_htlc_over_the_nominal_clock() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        // The nominal clock says long overdue, but the chain shows the
        // HTLC still 100 blocks from expiry (slow blocks, or a payer that
        // chose a larger final CLTV delta): nothing to do yet.
        let order = escrow_backed_order(Status::Active, now - 200_000)
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow = StubEscrow::ok().with_heights(900_000, Some(900_100));

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Active.to_string());
        assert!(escrow.canceled().is_empty());
    }

    #[tokio::test]
    async fn escrow_deadline_retries_transient_cancel_while_the_htlc_is_alive() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        // Past the nominal hard horizon (86_400s), but the chain shows the
        // accepted HTLC still alive (10 blocks left): a transient cancel
        // failure must NOT be taken as proof of an auto-refund — the order
        // stays eligible and is retried.
        let order = escrow_backed_order(Status::Active, now - 90_000)
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow = StubEscrow::failing("transport: connection refused")
            .with_heights(900_000, Some(900_010));

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Active.to_string());
        assert!(queued_actions_for(order.id).await.is_empty());
    }

    #[tokio::test]
    async fn escrow_deadline_fixes_state_when_no_htlc_backs_the_invoice() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        // LND reports no accepted HTLC behind the invoice: the escrow is
        // verifiably gone, so even a transient cancel error must not keep
        // the order looking live.
        let order = escrow_backed_order(Status::Active, now - 40_000)
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow =
            StubEscrow::failing("transport: connection refused").with_heights(900_000, None);

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Canceled.to_string());
    }

    #[tokio::test]
    async fn escrow_deadline_never_assumes_refund_without_chain_evidence() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        // No chain view at all and a transient cancel failure, way past
        // the nominal hard horizon: the old timestamp-based inference
        // would have marked the order canceled; now only chain evidence
        // (or an already-canceled report) may fix state.
        let order = escrow_backed_order(Status::Active, now - 90_000)
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow = StubEscrow::failing("transport: connection refused");

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Active.to_string());
    }

    #[tokio::test]
    async fn escrow_deadline_ignores_orders_inside_the_window() {
        let ctx = escrow_guardian_ctx(144, 24).await;
        let now = Utc::now().timestamp();
        // Only 1_000s into the 72_000s guard window: not due.
        let order = escrow_backed_order(Status::Active, now - 1_000)
            .create(ctx.pool())
            .await
            .unwrap();
        let mut escrow = StubEscrow::ok();

        enforce_escrow_deadline_pass(&ctx, &mut escrow)
            .await
            .unwrap();

        let updated = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(updated.status, Status::Active.to_string());
        assert!(escrow.canceled().is_empty());
    }

    // ── job smoke tests ──────────────────────────────────────────────────
    //
    // The jobs are infinite `tokio::spawn` loops; under a paused clock
    // their sleeps auto-advance, so a short virtual wait drives several
    // iterations against the (empty) migrated database. Tasks die with the
    // test runtime. LND-backed jobs exercise their startup-failure paths:
    // the default lightning settings point at unreadable cert/macaroon
    // paths, so `LndConnector::new()` fails fast without any network.

    #[tokio::test]
    async fn start_scheduler_spawns_all_jobs_without_panicking() {
        let ctx = migrated_ctx().await;

        // job_info_event_send unwraps the LN status global.
        let _ = LN_STATUS.set(crate::lightning::LnStatus {
            version: "test".to_string(),
            node_pubkey: "00".repeat(32),
            commit_hash: "test".to_string(),
            node_alias: "test-node".to_string(),
            chains: vec!["bitcoin".to_string()],
            networks: vec!["regtest".to_string()],
            uris: vec![],
        });
        // job_update_bitcoin_prices consults the global price manager; the
        // canonical test install (empty providers, 30s interval — below the
        // 60s floor) also exercises the interval clamp.
        let _ = PriceManager::from_settings(crate::price::PriceSettings {
            update_interval_seconds: 30,
            providers: std::collections::HashMap::new(),
            ..Default::default()
        })
        .expect("empty provider set builds")
        .install_global();

        // Push one message into the restore-session queue so the flush
        // job's send-failure/retry path runs (no Nostr relays reachable).
        MESSAGE_QUEUES
            .queue_restore_session_msg
            .write()
            .await
            .push((
                Message::new_order(Some(Uuid::new_v4()), None, None, Action::Canceled, None),
                ctx.keys().public_key(),
            ));

        // Pause only after the pool and globals exist: pool setup under a
        // paused clock trips sqlx's acquire timeout via auto-advance.
        tokio::time::pause();
        start_scheduler(ctx).await;

        // Let every loop take a few virtual-time laps (60s cadence jobs run
        // ~6 times; the 250ms flush loop drains its retry budget).
        tokio::time::sleep(tokio::time::Duration::from_secs(400)).await;

        // The flush job must have dropped the undeliverable message after
        // exhausting its retries.
        assert!(
            MESSAGE_QUEUES
                .queue_restore_session_msg
                .read()
                .await
                .is_empty(),
            "undeliverable restore-session message must be dropped after retries"
        );
    }

    /// The info job must republish as soon as the maintenance flag flips,
    /// not at the next interval tick.
    #[tokio::test]
    async fn info_wake_fires_on_maintenance_change_before_the_interval() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let state = crate::app::maintenance::MaintenanceState::load(&pool)
            .await
            .unwrap();
        let interval = tokio::time::Duration::from_secs(3600);

        let waiter = state.clone();
        let wake = tokio::spawn(async move { wait_for_info_wake(interval, &waiter).await });
        tokio::task::yield_now().await;
        state.set(&pool, true, None).await.unwrap();
        let reason = tokio::time::timeout(tokio::time::Duration::from_secs(1), wake)
            .await
            .expect("must not wait for the interval")
            .unwrap();
        assert_eq!(reason, InfoWake::MaintenanceChanged);
    }

    #[tokio::test]
    async fn info_wake_falls_back_to_the_interval() {
        tokio::time::pause();
        let state = crate::app::maintenance::MaintenanceState::new();
        let reason = wait_for_info_wake(tokio::time::Duration::from_secs(10), &state).await;
        assert_eq!(reason, InfoWake::Interval);
    }

    #[tokio::test]
    async fn rate_events_job_drains_the_rate_queue() {
        let ctx = migrated_ctx().await;
        // Seed a signed dummy event; the job publishes (best-effort, no
        // relays) and clears the queue.
        let keys = nostr_sdk::prelude::Keys::generate();
        let event = nostr_sdk::prelude::EventBuilder::new(nostr::event::Kind::TextNote, "rate")
            .finalize(&keys)
            .unwrap();
        MESSAGE_QUEUES.queue_order_rate.write().await.push(event);

        tokio::time::pause();
        job_update_rate_events(ctx).await;
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

        assert!(MESSAGE_QUEUES.queue_order_rate.read().await.is_empty());
    }

    #[tokio::test]
    async fn expiry_and_retry_jobs_iterate_on_empty_database() {
        let ctx = migrated_ctx().await;
        tokio::time::pause();
        job_expire_pending_older_orders(ctx.clone()).await;
        job_retry_failed_payments(ctx.clone()).await;
        job_refresh_active_pubkeys(ctx.clone()).await;
        job_reconcile_stranded_maker_bonds(ctx.clone()).await;
        job_relay_list(ctx).await;
        // Several virtual minutes: every loop body runs repeatedly.
        tokio::time::sleep(tokio::time::Duration::from_secs(200)).await;
    }

    #[tokio::test]
    async fn ln_backed_jobs_fail_fast_without_lnd() {
        let ctx = migrated_ctx().await;
        // Both return early (error log) because LndConnector::new() cannot
        // read the default cert/macaroon paths.
        job_cancel_orders(ctx.clone()).await;
        job_process_dev_fee_payment(ctx).await;
    }
}
