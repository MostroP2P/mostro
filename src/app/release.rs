use crate::config;
use crate::config::constants::DEV_FEE_LIGHTNING_ADDRESS;
use crate::config::MOSTRO_DB_PASSWORD;
use crate::db::{self};
use crate::lightning::LndConnector;
use crate::lnurl::resolv_ln_address;
use crate::nip33::{new_event, order_to_tags};
use crate::util::{
    enqueue_order_msg, get_keys, get_nostr_client, get_order, settle_seller_hold_invoice,
    update_order_event,
};

use argon2::password_hash::SaltString;
use config::settings::*;
use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;
use lnurl::lightning_address::LightningAddress;
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use rand;
use rand::rngs::OsRng;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::cmp::Ordering;
use std::str::FromStr;
use tokio::sync::mpsc::channel;
use tracing::info;

/// Check if order has failed payment retries
pub async fn check_failure_retries(
    order: &Order,
    request_id: Option<u64>,
) -> Result<Order, MostroError> {
    let mut order = order.clone();

    // Arc clone of db pool to use across threads
    let pool = get_db_pool();

    // Get max number of retries
    let ln_settings = Settings::get_ln();
    let retries_number = ln_settings.payment_attempts as i64;

    let is_first_failure = order.payment_attempts == 0;

    // Count payment retries up to limit
    order.count_failed_payment(retries_number);

    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Only send notification on first failure
    if is_first_failure {
        // Create payment failed payload with retry configuration
        let payment_failed_info = PaymentFailedInfo {
            payment_attempts: ln_settings.payment_attempts.saturating_sub(1),
            payment_retries_interval: ln_settings.payment_retries_interval,
        };

        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::PaymentFailed,
            Some(Payload::PaymentFailed(payment_failed_info)),
            buyer_pubkey,
            None,
        )
        .await;
    } else if order.payment_attempts >= retries_number {
        // Clone order
        let mut order_payment_failed = order.clone();
        // Update amount notified to the buyer
        let buyer_dev_fee = order.dev_fee / 2;
        order_payment_failed.amount = order_payment_failed
            .amount
            .saturating_sub(order.fee)
            .saturating_sub(buyer_dev_fee);
        if order_payment_failed.amount <= 0 {
            return Err(MostroCantDo(CantDoReason::InvalidAmount));
        }
        // Check errors
        if mostro_core::order::Kind::from_str(&order.kind).is_err() {
            return Err(MostroCantDo(CantDoReason::InvalidOrderKind));
        }
        // Check status
        if order_payment_failed.get_order_status().is_err() {
            return Err(MostroInternalErr(ServiceError::InvalidOrderStatus));
        }

        // Send message to buyer indicating payment failed
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::AddInvoice,
            Some(Payload::Order(SmallOrder::from(
                order_payment_failed.clone(),
            ))),
            buyer_pubkey,
            None,
        )
        .await;
    }

    // Update order
    let result = order
        .update(&pool)
        .await
        .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;
    Ok(result)
}

/// Handles the release action for an order, managing the release of funds and subsequent order flow.
///
/// This function is responsible for processing the release of funds in a trade, which is a critical
/// step in the order lifecycle. It verifies the seller's identity, manages the settlement of hold
/// invoices, and coordinates the creation of child orders for range orders. The function also
/// handles notifications to both buyer and seller about the release status.
///
/// # Arguments
///
/// * `msg` - The message containing the release request and associated metadata
/// * `event` - The unwrapped gift event containing the seller's signature and verification data
/// * `my_keys` - The Mostro node's keys used for signing events and messages
/// * `pool` - Database connection pool for order updates
/// * `ln_client` - Lightning network client for invoice settlement
///
/// # Returns
///
/// Returns a `Result<(), MostroError>` where:
/// * `Ok(())` indicates successful release of funds and order processing
/// * `Err(MostroError)` indicates an error occurred during the process
///
/// # Flow
///
/// 1. Validates the request:
///    - Verifies the seller's identity matches the order
///    - Checks if the order status allows for release
///
/// 2. Processes the release:
///    - Settles the seller's hold invoice
///    - Updates the order status to SettledHoldInvoice
///    - Notifies the buyer about the release
///
/// 3. Handles child orders (for range orders):
///    - Creates and processes child orders if applicable
///    - Sends notifications to next traders in the sequence
///
/// 4. Sends notifications:
///    - Notifies seller about hold invoice settlement
///    - Requests rating from seller
///    - Initiates payment to buyer
///
/// # Errors
///
/// This function may return the following errors:
/// * `MostroCantDo(CantDoReason::InvalidPeer)` - If the seller's identity doesn't match
/// * `MostroCantDo(CantDoReason::NotAllowedByStatus)` - If the order status doesn't allow release
/// * `MostroInternalErr(ServiceError::DbAccessError)` - If database operations fail
/// * `MostroInternalErr(ServiceError::NostrError)` - If there are issues with Nostr operations
/// * `MostroInternalErr(ServiceError::InvoiceInvalidError)` - If there are issues with the invoice
///
/// # Security Considerations
///
/// * Only the seller can release funds for their order
/// * The seller's identity is verified through the event signature
/// * Hold invoices are settled only after proper verification
pub async fn release_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let mut order = get_order(&msg, pool).await?;
    // Get seller pubkey hex
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    // We send a message to buyer indicating seller released funds
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Check if the pubkey is the seller pubkey - Only the seller can release funds
    if seller_pubkey != event.rumor.pubkey {
        return Err(MostroCantDo(CantDoReason::InvalidPeer));
    }

    // Check if order is in status fiat sent or dispute
    if order.check_status(Status::FiatSent).is_err() && order.check_status(Status::Dispute).is_err()
    {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    // Get next trade key
    let next_trade = msg
        .get_inner_message_kind()
        .get_next_trade_key()
        .map_err(MostroInternalErr)?;

    // Settle seller hold invoice
    settle_seller_hold_invoice(event, ln_client, Action::Released, false, &order).await?;
    // Update order event with status SettledHoldInvoice
    order = update_order_event(my_keys, Status::SettledHoldInvoice, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    enqueue_order_msg(
        None,
        Some(order.id),
        Action::Released,
        None,
        buyer_pubkey,
        None,
    )
    .await;

    // Handle child order for range orders
    if let Ok((Some(child_order), Some(event))) = get_child_order(order.clone(), my_keys).await {
        if let Ok(client) = get_nostr_client() {
            if client.send_event(&event).await.is_err() {
                tracing::warn!("Failed sending child order event for order id: {}. This may affect order synchronization", child_order.id)
            }
        }
        handle_child_order(child_order, &order, next_trade, pool, request_id)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    }

    // We send a HoldInvoicePaymentSettled message to seller, the client should
    // indicate *funds released* message to seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::HoldInvoicePaymentSettled,
        None,
        seller_pubkey,
        None,
    )
    .await;

    // We send a message to seller indicating seller released funds
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::Rate,
        None,
        seller_pubkey,
        None,
    )
    .await;

    // Finally we try to pay buyer's invoice
    let _ = do_payment(order, request_id).await;

    Ok(())
}

/// Helper function to store encrypted pubkey with optional salt
fn store_encrypted_pubkey(pubkey: &str, salt: Option<SaltString>) -> Result<String, MostroError> {
    CryptoUtils::store_encrypted(pubkey, MOSTRO_DB_PASSWORD.get(), salt).map_err(|_| {
        MostroInternalErr(ServiceError::EncryptionError(
            "Error storing encrypted pubkey".to_string(),
        ))
    })
}

/// Helper function to handle buy order case in child order creation
fn handle_buy_child_order(
    child_order: &mut Order,
    order: &Order,
    normal_buyer_idkey: Option<String>,
) -> Result<(Option<String>, Option<i64>), MostroError> {
    let next_buyer_pubkey = order.next_trade_pubkey.clone().ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(
            "Next trade buyer pubkey is missing".to_string(),
        ))
    })?;

    child_order.buyer_pubkey = Some(next_buyer_pubkey.clone());
    child_order.trade_index_buyer = order.next_trade_index;
    child_order.creator_pubkey = next_buyer_pubkey.clone();

    // Generate random salt if normal_buyer_idkey is Some
    let salt = if normal_buyer_idkey.is_some() {
        Some(SaltString::generate(&mut OsRng))
    } else {
        None
    };

    child_order.master_buyer_pubkey = Some(store_encrypted_pubkey(&next_buyer_pubkey, salt)?);

    // Clear next trade fields for buy order
    child_order.next_trade_index = None;
    child_order.next_trade_pubkey = None;

    Ok((
        child_order.buyer_pubkey.clone(),
        child_order.trade_index_buyer,
    ))
}

/// Helper function to handle sell order case in child order creation
fn handle_sell_child_order(
    child_order: &mut Order,
    next_trade: Option<(String, u32)>,
    normal_seller_idkey: Option<String>,
) -> Result<(Option<String>, Option<i64>), MostroError> {
    let (next_trade_pubkey, next_trade_index) = next_trade.ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(
            "Next trade seller pubkey is missing".to_string(),
        ))
    })?;

    let next_trade_pubkey = PublicKey::from_str(&next_trade_pubkey)
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;

    child_order.seller_pubkey = Some(next_trade_pubkey.to_string());
    child_order.trade_index_seller = Some(next_trade_index as i64);
    child_order.creator_pubkey = next_trade_pubkey.to_string();

    // Generate random salt if normal_seller_idkey is Some
    let salt = if normal_seller_idkey.is_some() {
        Some(SaltString::generate(&mut OsRng))
    } else {
        None
    };

    child_order.master_seller_pubkey = Some(store_encrypted_pubkey(
        &next_trade_pubkey.to_string(),
        salt,
    )?);

    Ok((
        child_order.seller_pubkey.clone(),
        child_order.trade_index_seller,
    ))
}

/// Manages the creation and update of child orders in a range order sequence.
///
/// This function handles the creation and setup of child orders for range orders, which are orders
/// that can be split into multiple smaller orders. It manages the encryption of pubkeys, sets up
/// trade indices, and handles notifications to the next trader in the sequence.
///
/// # Arguments
///
/// * `child_order` - The child order to be created/updated. This is a new order derived from the parent order.
/// * `order` - The parent order from which the child order is derived. Contains the original order details.
/// * `next_trade` - Optional tuple containing the next trader's information:
///   - First element: The public key of the next trader
///   - Second element: The trade index for the next trade
/// * `pool` - Database connection pool for storing the child order
/// * `request_id` - Optional request ID used for message queuing and tracking
///
/// # Returns
///
/// Returns a `Result<(), MostroError>` where:
/// * `Ok(())` indicates successful creation and setup of the child order
/// * `Err(MostroError)` indicates an error occurred during the process
///
/// # Flow
///
/// 1. Determines if users are in rating mode or full privacy mode
/// 2. Based on order type (buy/sell):
///    - For buy orders: Sets up buyer-specific fields and encrypts buyer pubkey
///    - For sell orders: Sets up seller-specific fields and encrypts seller pubkey
/// 3. Creates a new pending child order
/// 4. If next trade information is available:
///    - Enqueues a notification message to the next trader
/// 5. Stores the child order in the database
///
/// # Errors
///
/// This function may return the following errors:
/// * `MostroInternalErr(ServiceError::UnexpectedError)` - If the order type or creator is invalid
/// * `MostroInternalErr(ServiceError::DbAccessError)` - If database operations fail
/// * `MostroInternalErr(ServiceError::NostrError)` - If there are issues with Nostr operations
async fn handle_child_order(
    mut child_order: Order,
    order: &Order,
    next_trade: Option<(String, u32)>,
    pool: &Pool<Sqlite>,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    // Check if users are in rating mode or full privacy mode
    let (normal_buyer_idkey, normal_seller_idkey) = order
        .is_full_privacy_order(MOSTRO_DB_PASSWORD.get())
        .map_err(|_| {
            MostroInternalErr(ServiceError::UnexpectedError(
                "Error creating order event".to_string(),
            ))
        })?;

    let (notification_pubkey, new_trade_index) = if order.is_buy_order().is_ok()
        && order.buyer_pubkey.as_ref() == Some(&order.creator_pubkey)
    {
        handle_buy_child_order(&mut child_order, order, normal_buyer_idkey)?
    } else if order.is_sell_order().is_ok()
        && order.seller_pubkey.as_ref() == Some(&order.creator_pubkey)
    {
        handle_sell_child_order(&mut child_order, next_trade, normal_seller_idkey)?
    } else {
        return Err(MostroInternalErr(ServiceError::UnexpectedError(
            "Invalid order type or creator".to_string(),
        )));
    };

    // Prepare new pending child order
    let new_order = child_order.as_new_order();

    if let (Some(destination_pubkey), new_trade_index) = (notification_pubkey, new_trade_index) {
        // If we have next trade pubkey and index we can set them in child order
        enqueue_order_msg(
            request_id,
            new_order.id,
            Action::NewOrder,
            Some(Payload::Order(new_order)),
            PublicKey::from_str(&destination_pubkey).map_err(|_| {
                MostroInternalErr(ServiceError::NostrError("Invalid pubkey".to_string()))
            })?,
            new_trade_index,
        )
        .await;
    } else {
        return Err(MostroInternalErr(ServiceError::UnexpectedError(
            "Next trade index or pubkey is missing - user cannot be notified".to_string(),
        )));
    }

    // Create the child order in database
    child_order
        .create(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}

pub async fn do_payment(mut order: Order, request_id: Option<u64>) -> Result<(), MostroError> {
    let payment_request = match order.buyer_invoice.as_ref() {
        Some(req) => req.to_string(),
        _ => return Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
    };

    let ln_addr = LightningAddress::from_str(&payment_request);
    // Buyer receives order amount minus their Mostro fee share minus their dev fee share
    let buyer_dev_fee = (order.dev_fee / 2) as u64;

    let amount = (order.amount as u64)
        .checked_sub(order.fee as u64)
        .and_then(|a| a.checked_sub(buyer_dev_fee))
        .ok_or(MostroCantDo(CantDoReason::InvalidAmount))?;
    let payment_request = if let Ok(addr) = ln_addr {
        resolv_ln_address(&addr.to_string(), amount)
            .await
            .map_err(|_| MostroInternalErr(ServiceError::LnAddressParseError))?
    } else {
        payment_request
    };
    let mut ln_client_payment = LndConnector::new().await?;
    let (tx, mut rx) = channel(100);

    let payment_task = ln_client_payment.send_payment(&payment_request, amount as i64, tx);
    if let Err(paymement_result) = payment_task.await {
        info!("Error during ln payment : {}", paymement_result);
        if let Ok(failed_payment) = check_failure_retries(&order, request_id).await {
            info!(
                "Order id {} has {} failed payments retries",
                failed_payment.id, failed_payment.payment_attempts
            );
        }
    }

    // Get Mostro keys
    let my_keys =
        get_keys().map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Get buyer and seller pubkeys
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    let payment = {
        async move {
            // We redeclare vars to use inside this block
            // Receiving msgs from send_payment()
            while let Some(msg) = rx.recv().await {
                if let Ok(status) = PaymentStatus::try_from(msg.payment.status) {
                    match status {
                        PaymentStatus::Succeeded => {
                            info!(
                                "Order Id {}: Invoice with hash: {} paid!",
                                order.id, msg.payment.payment_hash
                            );
                            if let Err(e) =
                                payment_success(&mut order, buyer_pubkey, &my_keys, request_id)
                                    .await
                            {
                                tracing::error!(
                                    "Order Id {}: payment_success failed: {:?}",
                                    order.id,
                                    e
                                );
                            }
                        }
                        PaymentStatus::Failed => {
                            info!(
                                "Order Id {}: Invoice with hash: {} has failed!",
                                order.id, msg.payment.payment_hash
                            );

                            // Mark payment as failed
                            if let Ok(failed_payment) =
                                check_failure_retries(&order, request_id).await
                            {
                                info!(
                                    "Order id {} has {} failed payments retries",
                                    failed_payment.id, failed_payment.payment_attempts
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    };
    tokio::spawn(payment);
    Ok(())
}

async fn payment_success(
    order: &mut Order,
    buyer_pubkey: PublicKey,
    my_keys: &Keys,
    request_id: Option<u64>,
) -> Result<()> {
    tracing::info!("Order Id {}: payment_success starting", order.id);

    // Purchase completed message to buyer
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::PurchaseCompleted,
        None,
        buyer_pubkey,
        None,
    )
    .await;

    tracing::info!("Order Id {}: Getting DB connection", order.id);

    // Get db connection
    let pool = db::connect().await.map_err(|e| {
        tracing::error!(
            "Order Id {}: Failed to connect to database: {:?}",
            order.id,
            e
        );
        MostroInternalErr(ServiceError::DbAccessError(e.to_string()))
    })?;

    // Development fee will be processed asynchronously by scheduler
    if order.dev_fee > 0 {
        tracing::info!(
            "Order Id {}: Development fee payment ({} sats) will be processed by scheduler",
            order.id,
            order.dev_fee
        );
    }

    // Update order event to Success status
    let mut order_updated = update_order_event(my_keys, Status::Success, order)
        .await
        .map_err(|e| {
            tracing::error!(
                "Order Id {}: Failed to update order event to Success: {:?}",
                order.id,
                e
            );
            MostroInternalErr(ServiceError::NostrError(
                "Failed to update order event".to_string(),
            ))
        })?;

    // Reset failed payment flags after successful payment
    order_updated.failed_payment = false;
    order_updated.payment_attempts = 0;

    // Save to database
    order_updated.update(&pool).await.map_err(|e| {
        tracing::error!(
            "Order Id {}: Failed to save order to database: {:?}",
            order.id,
            e
        );
        MostroInternalErr(ServiceError::DbAccessError(e.to_string()))
    })?;

    tracing::info!(
        "Order Id {}: Order successfully updated to Success status",
        order.id
    );

    // Send dm to buyer to rate counterpart
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Rate,
        None,
        buyer_pubkey,
        None,
    )
    .await;

    Ok(())
}

/// Sends development fee to Mostro development Lightning Address
/// Returns payment hash on success, error on failure
/// Errors are non-fatal - logged but don't block order completion
pub async fn send_dev_fee_payment(order: &Order) -> Result<String, MostroError> {
    // Check if dev fee exists and is non-zero
    let dev_fee_amount = match order.dev_fee {
        fee if fee > 0 => fee,
        _ => return Ok(String::new()), // No dev fee to send
    };

    tracing::info!(
        "Order Id {}: Initiating development fee payment - amount: {} sats to: {}",
        order.id,
        dev_fee_amount,
        DEV_FEE_LIGHTNING_ADDRESS
    );

    tracing::info!(
        "Order Id {}: Resolving Lightning Address for dev fee payment",
        order.id
    );

    // Resolve Lightning Address to BOLT11 invoice
    let payment_request = resolv_ln_address(DEV_FEE_LIGHTNING_ADDRESS, dev_fee_amount as u64)
        .await
        .map_err(|e| {
            tracing::error!(
                "Order Id {}: Failed to resolve development Lightning Address: {:?}",
                order.id,
                e
            );
            MostroInternalErr(ServiceError::LnAddressParseError)
        })?;

    tracing::info!(
        "Order Id {}: Lightning Address resolved successfully, got invoice",
        order.id
    );

    if payment_request.is_empty() {
        tracing::error!(
            "Order Id {}: Lightning Address resolution returned empty invoice",
            order.id
        );
        return Err(MostroInternalErr(ServiceError::LnAddressParseError));
    }

    // Send payment via LND
    tracing::info!(
        "Order Id {}: Creating LND client for dev fee payment",
        order.id
    );
    let mut ln_client = LndConnector::new().await?;
    let (tx, mut rx) = channel(1);

    tracing::info!("Order Id {}: Sending dev fee payment via LND", order.id);
    ln_client
        .send_payment(&payment_request, dev_fee_amount, tx)
        .await?;

    tracing::info!(
        "Order Id {}: Waiting for dev fee payment result (30s timeout)",
        order.id
    );

    // Wait for payment result with 30-second timeout
    let payment_result = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv()).await;

    match payment_result {
        Ok(Some(msg)) => {
            if let Ok(status) = PaymentStatus::try_from(msg.payment.status) {
                match status {
                    PaymentStatus::Succeeded => {
                        let hash = msg.payment.payment_hash;
                        tracing::info!(
                            "Order Id {}: Development fee payment succeeded - hash: {}",
                            order.id,
                            hash
                        );
                        Ok(hash)
                    }
                    _ => {
                        tracing::error!(
                            "Order Id {}: Development fee payment failed - status: {:?}",
                            order.id,
                            status
                        );
                        Err(MostroInternalErr(ServiceError::LnPaymentError(format!(
                            "Payment failed: {:?}",
                            status
                        ))))
                    }
                }
            } else {
                Err(MostroInternalErr(ServiceError::LnPaymentError(
                    "Invalid payment status".to_string(),
                )))
            }
        }
        Ok(None) => {
            tracing::error!(
                "Order Id {}: Development fee payment channel closed unexpectedly",
                order.id
            );
            Err(MostroInternalErr(ServiceError::LnPaymentError(
                "Channel closed".to_string(),
            )))
        }
        Err(_) => {
            tracing::error!(
                "Order Id {}: Development fee payment timeout after 30 seconds",
                order.id
            );
            Err(MostroInternalErr(ServiceError::LnPaymentError(
                "Timeout".to_string(),
            )))
        }
    }
}

/// Check if order is range type
/// Add parent range id and update max amount
/// publish a new replaceable kind nostr event with the status updated
/// and update on local database the status and new event id
pub async fn get_child_order(
    order: Order,
    my_keys: &Keys,
) -> Result<(Option<Order>, Option<Event>), MostroError> {
    let (Some(max_amount), Some(min_amount)) = (order.max_amount, order.min_amount) else {
        return Ok((None, None));
    };

    if let Some(new_max) = max_amount.checked_sub(order.fiat_amount) {
        let mut new_order = create_base_order(&order)?;

        match new_max.cmp(&min_amount) {
            Ordering::Equal => {
                let (order, event) = order_for_equal(new_max, &mut new_order, my_keys).await?;
                return Ok((Some(order), Some(event)));
            }
            Ordering::Greater => {
                let (order, event) = order_for_greater(new_max, &mut new_order, my_keys).await?;
                return Ok((Some(order), Some(event)));
            }
            Ordering::Less => {
                return Ok((None, None));
            }
        }
    }

    Ok((None, None))
}

fn create_base_order(order: &Order) -> Result<Order, MostroError> {
    let mut new_order = order.clone();
    new_order.id = uuid::Uuid::new_v4();
    new_order.status = Status::Pending.to_string();
    new_order.amount = 0;
    new_order.hash = None;
    new_order.preimage = None;
    new_order.buyer_invoice = None;
    new_order.taken_at = 0;
    new_order.invoice_held_at = 0;
    new_order.range_parent_id = Some(order.id);

    match new_order.get_order_kind().map_err(MostroInternalErr)? {
        mostro_core::order::Kind::Sell => {
            new_order.buyer_pubkey = None;
            new_order.master_buyer_pubkey = None;
            new_order.trade_index_buyer = None;
        }
        mostro_core::order::Kind::Buy => {
            new_order.seller_pubkey = None;
            new_order.master_seller_pubkey = None;
            new_order.trade_index_seller = None;
        }
    }

    Ok(new_order)
}

async fn create_order_event(new_order: &mut Order, my_keys: &Keys) -> Result<Event, MostroError> {
    // Arc clone db pool to safe use across threads
    let pool = get_db_pool();

    // Extract user for rating tag
    let identity_pubkey = match new_order.is_sell_order() {
        Ok(_) => new_order
            .get_master_seller_pubkey(MOSTRO_DB_PASSWORD.get())
            .map_err(MostroInternalErr)?,
        Err(_) => new_order
            .get_master_buyer_pubkey(MOSTRO_DB_PASSWORD.get())
            .map_err(MostroInternalErr)?,
    };

    // If user has sent the order with his identity key means that he wants to be rate so we can just
    // check if we have identity key in db - if present we have to send reputation tags otherwise no.
    let tags = match crate::db::is_user_present(&pool, identity_pubkey).await {
        Ok(user) => order_to_tags(
            new_order,
            Some((user.total_rating, user.total_reviews, user.created_at)),
        )?,
        Err(_) => order_to_tags(new_order, Some((0.0, 0, 0)))?,
    };

    // Prepare new child order event for sending
    let event = if let Some(tags) = tags {
        new_event(my_keys, "", new_order.id.to_string(), tags)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?
    } else {
        return Err(MostroInternalErr(ServiceError::UnexpectedError(
            "Error creating order event".to_string(),
        )));
    };

    new_order.event_id = event.id.to_string();
    Ok(event)
}

async fn order_for_equal(
    new_max: i64,
    new_order: &mut Order,
    my_keys: &Keys,
) -> Result<(Order, Event), MostroError> {
    new_order.fiat_amount = new_max;
    new_order.max_amount = None;
    new_order.min_amount = None;
    let event = create_order_event(new_order, my_keys).await?;

    Ok((new_order.clone(), event))
}

async fn order_for_greater(
    new_max: i64,
    new_order: &mut Order,
    my_keys: &Keys,
) -> Result<(Order, Event), MostroError> {
    new_order.max_amount = Some(new_max);
    new_order.fiat_amount = 0;
    let event = create_order_event(new_order, my_keys).await?;

    Ok((new_order.clone(), event))
}
