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
use mostro_core::prelude::*;
use nostr_sdk::EventBuilder;
use nostr_sdk::{Kind as NostrKind, Tag};
use sqlx_crud::Crud;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use util::{enqueue_order_msg, get_nostr_relays, send_dm, update_order_event};

pub async fn start_scheduler(ctx: AppContext) {
    info!("Creating scheduler");

    job_expire_pending_older_orders(ctx.clone()).await;
    job_update_rate_events(ctx.clone()).await;
    job_cancel_orders(ctx.clone()).await;
    job_retry_failed_payments(ctx.clone()).await;
    job_process_dev_fee_payment(ctx.clone()).await;
    job_process_bond_payouts(ctx.clone()).await;
    job_info_event_send(ctx.clone()).await;
    job_relay_list(ctx.clone()).await;
    job_update_bitcoin_prices().await;
    job_flush_messages_queue(ctx.clone()).await;

    info!("Scheduler Started");
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
    let ln_status = LN_STATUS.get().unwrap();
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
                            if let Err(e) = ln_client.cancel_hold_invoice(hash).await {
                                error!("{e}");
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
                            // a re-entry sees no `Locked` bond, so it is a
                            // no-op (no duplicate notify).
                            if let Err(e) = order_updated.update(pool).await {
                                tracing::warn!(
                                    "scheduler_timeout: persist failed for order {} ({}); will retry next tick",
                                    order_id, e
                                );
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
                    // We update the order id with the new event_id
                    if let Ok(order_updated) =
                        crate::util::update_order_event(&keys, Status::Expired, order).await
                    {
                        let order_id = order_updated.id;
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
                                bond::release_bonds_for_order_or_warn(
                                    pool,
                                    order_id,
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
