use crate::app::bond;
use crate::app::context::AppContext;
use crate::app::dev_fee::run_dev_fee_cycle;
use crate::app::release::do_payment;
use crate::config;
use crate::db::*;
use crate::lightning::LndConnector;
use crate::price::PriceManager;
use crate::util;
use crate::LN_STATUS;
use crate::{Keys, PublicKey};

use chrono::{TimeDelta, Utc};
use config::*;
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::EventBuilder;
use nostr_sdk::{Kind as NostrKind, Tag};
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
        job_retry_failed_payments(ctx.clone()).await;
        job_process_dev_fee_payment(ctx.clone()).await;
        job_process_bond_payouts(ctx.clone()).await;
        job_reconcile_stranded_maker_bonds(ctx.clone()).await;
    }

    // Mode-agnostic jobs (the info event self-skips when LN status is absent).
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
                    if r.is_connected() {
                        relay_tags.push(Tag::relay_metadata(r.url().clone(), None))
                    }
                }
                if let Ok(relay_ev) = EventBuilder::new(NostrKind::RelayList, "")
                    .tags(relay_tags)
                    .sign_with_keys(&mostro_keys)
                {
                    let _ = client.send_event(&relay_ev).await;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_info_event_send(ctx: AppContext) {
    let mostro_keys = ctx.keys().clone();
    let client = ctx.nostr_client().clone();
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
            info!("Sending info about mostro");

            let tags = crate::nip33::info_to_tags(ln_status);
            let id = mostro_keys.public_key().to_string();

            let info_ev = match crate::nip33::new_info_event(&mostro_keys, "", id, tags) {
                Ok(info) => info,
                Err(e) => return error!("{e}"),
            };

            let _ = client.send_event(&info_ev).await;

            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
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

async fn notify_users_canceled_order(
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

    tracing::info!(
        "Notifying maker {} that taker {} canceled the order {}",
        maker_pubkey.to_string(),
        taker_pubkey.to_string(),
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
                            info!("Order Id {}: Funds returned to seller - buyer did not sent regular invoice in time", &order.id);
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
                                "Republishing order Id {}, not received regular invoice in time",
                                order.id
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
                                    "Canceled order Id {}, not received regular invoice in time",
                                    order.id
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
                            println!("Edited order: {:?}", edited_order);
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
                                bond::release_bonds_for_order_or_warn(
                                    pool,
                                    order_id,
                                    "maker_bond_expiry",
                                )
                                .await;
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

    // On daemon restart the set is empty so each order gets re-checked once.
    let mut confirmed: HashSet<uuid::Uuid> = HashSet::new();

    tokio::spawn(async move {
        let pool = ctx.pool();
        loop {
            run_dev_fee_cycle(pool, &mut ln_client, &mut confirmed).await;
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
        nostr_sdk::Keys::generate().public_key().to_hex()
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

    #[tokio::test]
    async fn rate_events_job_drains_the_rate_queue() {
        let ctx = migrated_ctx().await;
        // Seed a signed dummy event; the job publishes (best-effort, no
        // relays) and clears the queue.
        let keys = nostr_sdk::Keys::generate();
        let event = nostr_sdk::EventBuilder::text_note("rate")
            .sign_with_keys(&keys)
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
