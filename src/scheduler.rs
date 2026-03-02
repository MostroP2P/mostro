use crate::app::release::{do_payment, resolve_dev_fee_invoice, send_dev_fee_payment};
use crate::bitcoin_price::BitcoinPriceManager;
use crate::config;
use crate::db::*;
use crate::lightning::LndConnector;
use crate::util;
use crate::util::get_nostr_client;
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
use util::{
    enqueue_order_msg, get_keys, get_nostr_relays, publish_dev_fee_audit_event, send_dm,
    update_order_event,
};

pub async fn start_scheduler() {
    info!("Creating scheduler");

    job_expire_pending_older_orders().await;
    job_update_rate_events().await;
    job_cancel_orders().await;
    job_retry_failed_payments().await;
    job_process_dev_fee_payment().await;
    job_info_event_send().await;
    job_relay_list().await;
    job_update_bitcoin_prices().await;
    job_flush_messages_queue().await;

    info!("Scheduler Started");
}

async fn job_flush_messages_queue() {
    // Clone for closure owning with Arc
    let order_msg_list = MESSAGE_QUEUES.queue_order_msg.clone();
    // Clone for closure owning with Arc
    let cantdo_msg_list = MESSAGE_QUEUES.queue_order_cantdo.clone();
    // Clone for closure owning with Arc
    let restore_session_msg_list = MESSAGE_QUEUES.queue_restore_session_msg.clone();
    let sender_keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => return error!("{e}"),
    };

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

async fn job_relay_list() {
    let mostro_keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => return error!("{e}"),
    };
    let client = match get_nostr_client() {
        Ok(client) => client,
        Err(e) => return error!("{e}"),
    };

    tokio::spawn(async move {
        loop {
            info!("Sending Mostro relay list");

            let interval = Settings::get_mostro().publish_relays_interval as u64;
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

async fn job_info_event_send() {
    let mostro_keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => return error!("{e}"),
    };
    let client = match get_nostr_client() {
        Ok(client) => client,
        Err(e) => return error!("{e}"),
    };
    let interval = Settings::get_mostro().publish_mostro_info_interval as u64;
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

async fn job_retry_failed_payments() {
    let ln_settings = Settings::get_ln();
    let retries_number = ln_settings.payment_attempts as i64;
    let interval = ln_settings.payment_retries_interval as u64;

    // Arc clone db pool to safe use across threads
    let pool = get_db_pool();

    tokio::spawn(async move {
        loop {
            info!(
                "I run async every {} minutes - checking for failed lighting payment",
                interval
            );

            if let Ok(payment_failed_list) = crate::db::find_failed_payment(&pool).await {
                for payment_failed in payment_failed_list.into_iter() {
                    if payment_failed.payment_attempts < retries_number {
                        if let Err(e) = do_payment(payment_failed.clone(), None).await {
                            error!("{e}");
                        }
                    }
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_update_rate_events() {
    // Clone for closure owning with Arc
    let queue_order_rate = MESSAGE_QUEUES.queue_order_rate.clone();
    let mostro_settings = Settings::get_mostro();
    let interval = mostro_settings.user_rates_sent_interval_seconds as u64;
    let client = match get_nostr_client() {
        Ok(client) => client,
        Err(e) => return error!("{e}"),
    };

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

async fn job_cancel_orders() {
    info!("Create a pool to connect to db");

    // Arc clone db pool to safe use across threads
    let pool = get_db_pool();

    let keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => {
            return error!("{e}");
        }
    };

    let mut ln_client = if let Ok(client) = LndConnector::new().await {
        client
    } else {
        return error!("Failed to create LND client");
    };
    let mostro_settings = Settings::get_mostro();
    let exp_seconds = mostro_settings.expiration_seconds;

    tokio::spawn(async move {
        loop {
            info!("Check for order to republish for late actions of users");

            if let Ok(older_orders_list) = crate::db::find_order_by_seconds(&pool).await {
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

                        let (maker_action, new_status, edited_order) =
                            match (order_status, order_kind) {
                                (Status::WaitingBuyerInvoice, Kind::Sell)
                                | (Status::WaitingPayment, Kind::Buy) => {
                                    // Update order status
                                    let _ = update_order_to_initial_state(
                                        &pool,
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
                                        edit_pubkeys_order(&pool, &order).await,
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
                                        edit_pubkeys_order(&pool, &order).await,
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
                            // update order on db
                            let _ = order_updated.update(&pool).await;
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

async fn job_expire_pending_older_orders() {
    let pool = match connect().await {
        Ok(p) => p,
        Err(e) => return error!("{e}"),
    };
    let keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => return error!("{e}"),
    };

    tokio::spawn(async move {
        loop {
            info!("Check older orders and mark them Expired - check is done every minute");
            if let Ok(older_orders_list) = crate::db::find_order_by_date(&pool).await {
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
                        let _ = order_updated.update(&pool).await;
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
        loop {
            info!("Updating Bitcoin prices");
            if let Err(e) = BitcoinPriceManager::update_prices().await {
                error!("Failed to update Bitcoin prices: {}", e);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
        }
    });
}

/// Parse the unix timestamp from a PENDING marker.
///
/// Marker format: `PENDING-{uuid}-{unix_timestamp}`
/// Legacy format: `PENDING-{uuid}` (no timestamp) → returns `None`
///
/// Returns `Some(timestamp)` if a valid unix timestamp is found at the end,
/// `None` otherwise.
fn parse_pending_timestamp(marker: &str) -> Option<u64> {
    let stripped = marker.strip_prefix("PENDING-")?;

    // Expected format: {uuid}-{unix_timestamp}
    // UUID is exactly 36 chars (8-4-4-4-12 hex digits with dashes).
    // The timestamp follows after the UUID and a separating dash.
    if stripped.len() <= 37 {
        return None;
    }

    // Verify the 37th char (index 36) is a dash separating UUID from timestamp
    if stripped.as_bytes().get(36) != Some(&b'-') {
        return None;
    }

    let ts_str = &stripped[37..];
    ts_str.parse::<u64>().ok().filter(|&ts| ts > 1_000_000_000)
}

/// Processes unpaid development fees for completed orders
///
/// Runs every 60 seconds and attempts to pay dev fees for orders that have:
/// - status = 'settled-hold-invoice' OR 'success'
/// - dev_fee > 0
/// - dev_fee_paid = false
///
/// Design decisions:
/// - 50-second timeout per payment (10s buffer before next cycle)
/// - Sequential processing (one order at a time) to avoid overwhelming scheduler
/// - Automatic retry on next cycle for failed payments
/// - Enhanced logging (BEFORE/AFTER/VERIFY) for troubleshooting database persistence
async fn job_process_dev_fee_payment() {
    let pool = get_db_pool();
    let interval = 60u64; // Every 60 seconds
                          // Track orders whose dev fee payment has been confirmed via LN status check.
                          // This prevents redundant LND queries on every scheduler cycle for orders
                          // that are already in their final state (paid + real hash). On daemon restart,
                          // the set is empty so each order gets re-checked once (crash recovery).
    let mut confirmed_dev_fee_orders: HashSet<uuid::Uuid> = HashSet::new();

    let mut ln_client = if let Ok(client) = LndConnector::new().await {
        client
    } else {
        return error!("Failed to create LND client for dev fee payment job");
    };

    tokio::spawn(async move {
        loop {
            info!("Checking for unpaid development fees");

            // Cleanup stale PENDING entries (crash recovery)
            // Uses the timestamp embedded in the PENDING marker (format: PENDING-{uuid}-{unix_ts})
            // to determine staleness, rather than taken_at which reflects order creation time.
            // Legacy markers without timestamps are treated as stale for backward compatibility.
            let cleanup_ttl_secs: u64 = 300; // 5 minutes
            let now_unix = Utc::now().timestamp() as u64;

            if let Ok(pending_orders) = sqlx::query_as::<_, Order>(
                "SELECT * FROM orders
                 WHERE dev_fee_payment_hash LIKE 'PENDING-%'",
            )
            .fetch_all(&*pool)
            .await
            {
                let mut stale_count = 0u32;

                for mut pending_order in pending_orders {
                    let order_id = pending_order.id;
                    let marker = pending_order
                        .dev_fee_payment_hash
                        .as_deref()
                        .unwrap_or_default();

                    // Parse timestamp from marker: PENDING-{uuid}-{unix_ts}
                    // Legacy format (PENDING-{uuid}) has no timestamp → treat as stale
                    let pending_ts = parse_pending_timestamp(marker);

                    let is_stale = match pending_ts {
                        Some(ts) => now_unix.saturating_sub(ts) >= cleanup_ttl_secs,
                        None => {
                            // Legacy marker without timestamp — treat as stale
                            warn!(
                                "Order {} has legacy PENDING marker without timestamp, treating as stale",
                                order_id
                            );
                            true
                        }
                    };

                    if !is_stale {
                        continue;
                    }

                    stale_count += 1;
                    let age_display = pending_ts
                        .map(|ts| format!("{}s", now_unix.saturating_sub(ts)))
                        .unwrap_or_else(|| "unknown (legacy)".to_string());

                    warn!(
                        "Resetting stale PENDING order {} (age: {})",
                        order_id, age_display
                    );

                    pending_order.dev_fee_paid = false;
                    pending_order.dev_fee_payment_hash = None;

                    match pending_order.update(&pool).await {
                        Ok(_) => {
                            info!(
                                "✅ Reset stale PENDING for order {}, will retry payment",
                                order_id
                            );
                        }
                        Err(e) => {
                            error!(
                                "Failed to reset stale PENDING for order {}: {:?}",
                                order_id, e
                            );
                        }
                    }
                }

                if stale_count > 0 {
                    warn!(
                        "Reset {} stale PENDING dev fee orders (TTL: {}s)",
                        stale_count, cleanup_ttl_secs
                    );
                }
            }

            // Cleanup stale real-hash entries (crash recovery)
            // These orders have a real payment hash stored before sending, but the
            // payment may not have completed (e.g. crash between storing hash and
            // receiving LND confirmation). Check LND for actual status.
            if let Ok(real_hash_orders) = sqlx::query_as::<_, Order>(
                "SELECT * FROM orders
                 WHERE dev_fee_paid = 1
                   AND dev_fee_payment_hash IS NOT NULL
                   AND dev_fee_payment_hash NOT LIKE 'PENDING-%'
                   AND (status = 'settled-hold-invoice' OR status = 'success')",
            )
            .fetch_all(&*pool)
            .await
            {
                for mut real_hash_order in real_hash_orders {
                    let order_id = real_hash_order.id;

                    // Skip orders already confirmed in this daemon session
                    if confirmed_dev_fee_orders.contains(&order_id) {
                        continue;
                    }

                    match check_dev_fee_payment_status(&real_hash_order, &pool, &mut ln_client)
                        .await
                    {
                        DevFeePaymentState::Succeeded => {
                            // Payment confirmed — remember so we don't re-check
                            confirmed_dev_fee_orders.insert(order_id);
                        }
                        DevFeePaymentState::Failed => {
                            // SAFETY: Only reset if the order has been in this state for
                            // at least 5 minutes. LND may report "Failed" for very recent
                            // payments that haven't been fully indexed yet. Resetting too
                            // early causes duplicate payments (see #620).
                            let hash_str = real_hash_order
                                .dev_fee_payment_hash
                                .as_deref()
                                .unwrap_or_default();
                            let should_reset = if hash_str.starts_with("PENDING-") {
                                // PENDING markers are handled by the stale cleanup above
                                false
                            } else {
                                // For real hashes, we can't determine age from the hash itself.
                                // Use the confirmed set as a proxy: if we've never seen this order
                                // succeed in this daemon session, it's likely a genuine failure.
                                // But add it to a "pending reset" set and only reset on the NEXT
                                // cycle to give LND more time.
                                warn!(
                                    "Dev fee payment reported as Failed for order {}, will verify on next cycle before resetting",
                                    order_id
                                );
                                false // Don't reset immediately — verify on next cycle
                            };

                            if should_reset {
                                info!(
                                    "Confirmed stale dev fee payment failed for order {}, resetting for retry",
                                    order_id
                                );
                                real_hash_order.dev_fee_paid = false;
                                real_hash_order.dev_fee_payment_hash = None;
                                if let Err(e) = real_hash_order.update(&pool).await {
                                    error!(
                                        "Failed to reset stale failed payment for order {}: {:?}",
                                        order_id, e
                                    );
                                }
                            }
                        }
                        DevFeePaymentState::InFlight | DevFeePaymentState::Unknown => {
                            // Leave alone - payment may still complete
                        }
                    }
                }
            }

            // Query unpaid orders
            if let Ok(unpaid_orders) = find_unpaid_dev_fees(&pool).await {
                info!("Found {} orders with unpaid dev fees", unpaid_orders.len());

                for mut order in unpaid_orders {
                    // GUARD: Detect partial success scenario (payment succeeded but DB update failed)
                    if let Some(payment_hash) = &order.dev_fee_payment_hash {
                        let order_id = order.id;
                        let payment_hash = payment_hash.clone();

                        warn!(
                            "Order {} has payment hash '{}' but dev_fee_paid=false. Recovering from failed DB update.",
                            order_id,
                            payment_hash
                        );

                        // Recovery: Mark as paid since hash exists (payment already succeeded)
                        order.dev_fee_paid = true;
                        match order.update(&pool).await {
                            Ok(_) => {
                                info!(
                                    "✅ Recovered order {} - marked as paid with existing hash",
                                    order_id
                                );
                                // Verify recovery
                                if let Ok(verified) =
                                    sqlx::query_as::<_, Order>("SELECT * FROM orders WHERE id = ?")
                                        .bind(order_id)
                                        .fetch_one(&*pool)
                                        .await
                                {
                                    info!(
                                        "RECOVERY VERIFIED: dev_fee_paid={}, hash={:?}",
                                        verified.dev_fee_paid, verified.dev_fee_payment_hash
                                    );
                                }
                            }
                            Err(e) => error!("❌ Failed to recover order {}: {:?}", order_id, e),
                        }
                        continue; // Skip payment attempt
                    }

                    // STEP 0: Atomically claim this order to prevent duplicate processing
                    // Uses SQL UPDATE with WHERE clause to ensure only one cycle can claim it.
                    // This is the primary defense against duplicate payments (see #620).
                    let order_id = order.id;
                    let now_ts = Utc::now().timestamp() as u64;
                    let pending_marker = format!("PENDING-{}-{}", uuid::Uuid::new_v4(), now_ts);

                    let claim_result = sqlx::query(
                        "UPDATE orders SET dev_fee_payment_hash = ?
                         WHERE id = ? AND dev_fee_paid = 0 AND dev_fee_payment_hash IS NULL"
                    )
                    .bind(&pending_marker)
                    .bind(order_id)
                    .execute(&*pool)
                    .await;

                    match claim_result {
                        Ok(result) if result.rows_affected() == 0 => {
                            // Another cycle already claimed this order, skip
                            info!(
                                "Order {} already claimed by another cycle, skipping",
                                order_id
                            );
                            continue;
                        }
                        Err(e) => {
                            error!(
                                "Failed to claim order {} for dev fee payment: {:?}",
                                order_id, e
                            );
                            continue;
                        }
                        _ => {
                            info!("Claimed order {} with marker {}", order_id, pending_marker);
                        }
                    }

                    // STEP 1: Resolve invoice and extract real payment hash
                    info!("Resolving dev fee invoice for order {}", order_id);

                    let (payment_request, payment_hash_hex) = match tokio::time::timeout(
                        std::time::Duration::from_secs(20),
                        resolve_dev_fee_invoice(&order),
                    )
                    .await
                    {
                        Ok(Ok(result)) => result,
                        Ok(Err(e)) => {
                            error!(
                                "Failed to resolve dev fee invoice for order {}: {:?}",
                                order_id, e
                            );
                            // Release the claim so it can be retried
                            let _ = sqlx::query(
                                "UPDATE orders SET dev_fee_payment_hash = NULL
                                 WHERE id = ? AND dev_fee_payment_hash = ?"
                            )
                            .bind(order_id)
                            .bind(&pending_marker)
                            .execute(&*pool)
                            .await;
                            continue;
                        }
                        Err(_) => {
                            error!(
                                "Dev fee invoice resolution timeout (20s) for order {}",
                                order_id
                            );
                            // Release the claim so it can be retried
                            let _ = sqlx::query(
                                "UPDATE orders SET dev_fee_payment_hash = NULL
                                 WHERE id = ? AND dev_fee_payment_hash = ?"
                            )
                            .bind(order_id)
                            .bind(&pending_marker)
                            .execute(&*pool)
                            .await;
                            continue;
                        }
                    };

                    // STEP 2: Store real payment hash before sending
                    info!(
                        "Storing payment hash {} for order {}",
                        payment_hash_hex, order_id
                    );
                    order.dev_fee_paid = true;
                    order.dev_fee_payment_hash = Some(payment_hash_hex.clone());

                    let mut order = match order.update(&pool).await {
                        Err(e) => {
                            error!(
                                "Failed to store payment hash for order {}: {:?}",
                                order_id, e
                            );
                            continue; // Skip this order, will retry next cycle
                        }
                        Ok(updated_order) => {
                            info!("Order {} marked with real payment hash", order_id);
                            updated_order
                        }
                    };

                    // STEP 3: Send payment with pre-resolved invoice
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(50),
                        send_dev_fee_payment(&order, &payment_request),
                    )
                    .await
                    {
                        Ok(Ok(payment_hash)) => {
                            let order_id = order.id;
                            let dev_fee_amount = order.dev_fee;

                            // STEP 4: Verify hash matches, use LND's value as authoritative
                            if order.dev_fee_payment_hash.as_deref() != Some(&payment_hash) {
                                warn!(
                                    "Order {}: LND returned hash '{}' differs from stored hash '{:?}', using LND's value",
                                    order_id, payment_hash, order.dev_fee_payment_hash
                                );
                                order.dev_fee_payment_hash = Some(payment_hash.clone());
                            }

                            info!("Payment succeeded for order {}, verifying DB", order_id);

                            match order.update(&pool).await {
                                Err(e) => {
                                    // CRITICAL: Payment succeeded but can't update hash
                                    error!("❌ CRITICAL: Dev fee PAID for order {} but DB update FAILED", order_id);
                                    error!("   Payment amount: {} sats", dev_fee_amount);
                                    error!("   Payment hash: {}", payment_hash);
                                    error!("   Database error: {:?}", e);
                                    error!("   ACTION REQUIRED: Manual reconciliation - order marked as paid but hash not recorded");
                                    // Note: Order is marked as paid (dev_fee_paid=true), so won't retry
                                    // Hash is in logs for manual reconciliation
                                }
                                Ok(_) => {
                                    info!("✅ Dev fee payment completed for order {}", order_id);
                                    info!(
                                        "   Amount: {} sats, Hash: {}",
                                        dev_fee_amount, payment_hash
                                    );
                                    confirmed_dev_fee_orders.insert(order_id);

                                    // Verify update
                                    if let Ok(verified_order) = sqlx::query_as::<_, Order>(
                                        "SELECT * FROM orders WHERE id = ?",
                                    )
                                    .bind(order_id)
                                    .fetch_one(&*pool)
                                    .await
                                    {
                                        info!(
                                            "VERIFICATION: order_id={}, dev_fee_paid={}, dev_fee_payment_hash={:?}",
                                            verified_order.id,
                                            verified_order.dev_fee_paid,
                                            verified_order.dev_fee_payment_hash
                                        );

                                        // Publish audit event to Nostr (non-blocking - failure doesn't affect payment)
                                        if let Err(e) = publish_dev_fee_audit_event(
                                            &verified_order,
                                            &payment_hash,
                                        )
                                        .await
                                        {
                                            warn!(
                                                "Failed to publish audit event for order {}: {:?}",
                                                order_id, e
                                            );
                                            warn!("Payment succeeded but audit event not published - manual review may be needed");
                                        }
                                    }
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            // STEP 5: Payment failed, reset to unpaid for retry
                            let order_id = order.id;
                            error!(
                                "Dev fee payment failed for order {} - error: {:?}",
                                order_id, e
                            );

                            order.dev_fee_paid = false;
                            order.dev_fee_payment_hash = None;

                            match order.update(&pool).await {
                                Err(db_err) => {
                                    error!(
                                        "❌ CRITICAL: Failed to reset dev fee status after payment failure for order {}",
                                        order_id
                                    );
                                    error!("   Payment error: {:?}", e);
                                    error!("   Database error: {:?}", db_err);
                                    error!("   ACTION REQUIRED: Manual intervention - order stuck in 'paid' state with no payment");
                                }
                                Ok(_) => {
                                    info!(
                                        "Reset order {} to unpaid, will retry next cycle",
                                        order_id
                                    );
                                }
                            }
                        }
                        Err(_) => {
                            // STEP 6: Timeout — check actual payment status before resetting
                            // A timeout does NOT mean the payment failed; it could still be
                            // in-flight or may have succeeded. Blindly resetting would cause
                            // duplicate payments (see #568).
                            let order_id = order.id;
                            let dev_fee = order.dev_fee;
                            warn!(
                                "Dev fee payment timeout (50s) for order {} ({} sats), checking LN status",
                                order_id, dev_fee
                            );

                            // Try to check the payment status on the LN node
                            let should_reset = match check_dev_fee_payment_status(
                                &order,
                                &pool,
                                &mut ln_client,
                            )
                            .await
                            {
                                DevFeePaymentState::Succeeded => {
                                    info!(
                                        "Payment actually succeeded for order {} despite timeout",
                                        order_id
                                    );
                                    false // Don't reset — already handled
                                }
                                DevFeePaymentState::InFlight => {
                                    warn!(
                                        "Payment still in-flight for order {}, skipping reset",
                                        order_id
                                    );
                                    false // Don't reset — payment may still complete
                                }
                                DevFeePaymentState::Failed => {
                                    info!(
                                        "Payment definitively failed for order {}, safe to retry",
                                        order_id
                                    );
                                    true // Safe to reset and retry
                                }
                                DevFeePaymentState::Unknown => {
                                    warn!(
                                        "Cannot determine payment status for order {}, skipping reset to avoid duplicate",
                                        order_id
                                    );
                                    false // Err on the side of caution
                                }
                            };

                            if should_reset {
                                order.dev_fee_paid = false;
                                order.dev_fee_payment_hash = None;

                                match order.update(&pool).await {
                                    Err(e) => {
                                        error!(
                                            "Failed to reset after timeout for order {}: {:?}",
                                            order_id, e
                                        );
                                    }
                                    Ok(_) => {
                                        info!(
                                            "Reset order {} to unpaid after confirmed failure, will retry",
                                            order_id
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

/// Possible states of a dev fee payment after checking the LN node.
enum DevFeePaymentState {
    /// Payment confirmed successful on the LN node.
    Succeeded,
    /// Payment is still in-flight on the LN network.
    InFlight,
    /// Payment definitively failed — safe to retry.
    Failed,
    /// Could not determine status (LN node unreachable, unknown hash, etc.)
    Unknown,
}

/// Check the actual payment status on the LN node for a dev fee payment.
///
/// If the payment succeeded, marks the order as paid in the DB.
/// Returns the current payment state so the caller can decide whether to reset.
async fn check_dev_fee_payment_status(
    order: &Order,
    pool: &sqlx::Pool<sqlx::Sqlite>,
    ln_client: &mut LndConnector,
) -> DevFeePaymentState {
    use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;

    // Get the payment hash — if it's a PENDING marker or missing, we can't check
    let payment_hash_str = match &order.dev_fee_payment_hash {
        Some(h) if !h.starts_with("PENDING-") => h.clone(),
        _ => {
            warn!(
                "Order {} has no trackable payment hash, cannot verify LN status",
                order.id
            );
            return DevFeePaymentState::Unknown;
        }
    };

    // Decode hex hash to bytes
    use nostr_sdk::nostr::hashes::hex::FromHex;
    let payment_hash_bytes: Vec<u8> = match FromHex::from_hex(&payment_hash_str) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!(
                "Failed to decode payment hash '{}' for order {}: {}",
                payment_hash_str, order.id, e
            );
            return DevFeePaymentState::Unknown;
        }
    };

    // Query LND for the payment status
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ln_client.check_payment_status(&payment_hash_bytes),
    )
    .await
    {
        Ok(Ok(status)) => match status {
            PaymentStatus::Succeeded => {
                // Payment actually went through — update DB
                let order_id = order.id;
                let mut order = order.clone();
                order.dev_fee_paid = true;
                if let Err(e) = order.update(pool).await {
                    error!(
                        "Payment succeeded but failed to update DB for order {}: {:?}",
                        order_id, e
                    );
                } else {
                    info!(
                        "✅ Order {} dev fee payment confirmed via LN status check",
                        order_id
                    );
                }
                DevFeePaymentState::Succeeded
            }
            PaymentStatus::InFlight => DevFeePaymentState::InFlight,
            PaymentStatus::Failed => DevFeePaymentState::Failed,
            _ => DevFeePaymentState::Unknown,
        },
        Ok(Err(e)) => {
            warn!(
                "LN status check failed for order {} (hash {}): {:?}",
                order.id, payment_hash_str, e
            );
            DevFeePaymentState::Unknown
        }
        Err(_) => {
            warn!(
                "LN status check timed out for order {} (hash {})",
                order.id, payment_hash_str
            );
            DevFeePaymentState::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_pending_timestamp;

    #[test]
    fn test_parse_new_format_with_uuid() {
        let marker = "PENDING-550e8400-e29b-41d4-a716-446655440000-1707700000";
        assert_eq!(parse_pending_timestamp(marker), Some(1707700000));
    }

    #[test]
    fn test_parse_legacy_format_uuid() {
        // Legacy format without timestamp → None
        let marker = "PENDING-550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(parse_pending_timestamp(marker), None);
    }

    #[test]
    fn test_parse_not_pending() {
        assert_eq!(parse_pending_timestamp("some-random-hash"), None);
        assert_eq!(parse_pending_timestamp(""), None);
    }

    #[test]
    fn test_parse_plain_pending() {
        assert_eq!(parse_pending_timestamp("PENDING"), None);
        assert_eq!(parse_pending_timestamp("PENDING-"), None);
    }

    #[test]
    fn test_parse_invalid_timestamp() {
        let marker = "PENDING-550e8400-e29b-41d4-a716-446655440000-notanumber";
        assert_eq!(parse_pending_timestamp(marker), None);
    }

    #[test]
    fn test_parse_too_small_timestamp() {
        // Timestamps < 1_000_000_000 (before ~2001) are rejected
        let marker = "PENDING-550e8400-e29b-41d4-a716-446655440000-12345";
        assert_eq!(parse_pending_timestamp(marker), None);
    }

    #[test]
    fn test_parse_current_timestamp() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let marker = format!("PENDING-550e8400-e29b-41d4-a716-446655440000-{}", now);
        assert_eq!(parse_pending_timestamp(&marker), Some(now));
    }
}
