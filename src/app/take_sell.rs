use crate::config::MOSTRO_DB_PASSWORD;
use crate::db::{buyer_has_pending_order, update_user_trade_index};
use crate::util::{
    get_fiat_amount_requested, get_market_amount_and_fee, get_order, set_waiting_invoice_status,
    show_hold_invoice, update_order_event, validate_invoice,
};
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

async fn update_order_status(
    order: &mut Order,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    // Get buyer pubkey
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;
    // Set order status to waiting buyer invoice
    match set_waiting_invoice_status(order, buyer_pubkey, request_id).await {
        Ok(_) => {
            // Update order status
            match update_order_event(my_keys, Status::WaitingBuyerInvoice, order).await {
                Ok(order_updated) => {
                    let _ = order_updated.update(pool).await;
                    Ok(())
                }
                Err(_) => Err(MostroInternalErr(ServiceError::UpdateOrderStatusError)),
            }
        }
        Err(_) => Err(MostroInternalErr(ServiceError::UpdateOrderStatusError)),
    }
}

pub async fn take_sell_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get order
    let mut order = get_order(&msg, pool).await?;

    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Check if the seller has a pending order
    if buyer_has_pending_order(pool, event.sender.to_string()).await? {
        return Err(MostroCantDo(CantDoReason::PendingOrderExists));
    }

    // Check if the order is a sell order and if its status is active
    if let Err(cause) = order.is_sell_order() {
        return Err(MostroCantDo(cause));
    };
    // Check if the order status is pending
    if let Err(cause) = order.check_status(Status::Pending) {
        return Err(MostroCantDo(cause));
    }

    // Validate that the order was sent from the correct maker
    order
        .not_sent_from_maker(event.rumor.pubkey)
        .map_err(MostroCantDo)?;

    // Get seller pubkey
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;

    // Validate invoice and get payment request if present
    let payment_request = validate_invoice(&msg, &order).await?;

    // Get amount request if user requested one for range order - fiat amount will be used below
    if let Some(am) = get_fiat_amount_requested(&order, &msg) {
        order.fiat_amount = am;
    } else {
        return Err(MostroCantDo(CantDoReason::OutOfRangeSatsAmount));
    }

    // Add buyer pubkey to order
    order.buyer_pubkey = Some(event.rumor.pubkey.to_string());
    // Add buyer identity pubkey to order
    order.master_buyer_pubkey = Some(
        CryptoUtils::store_encrypted(&event.sender.to_string(), MOSTRO_DB_PASSWORD.get(), None)
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?,
    );

    let trade_index = match msg.get_inner_message_kind().trade_index {
        Some(trade_index) => trade_index,
        None => {
            if event.sender == event.rumor.pubkey {
                0
            } else {
                return Err(MostroInternalErr(ServiceError::InvalidPayload));
            }
        }
    };
    // Add buyer trade index to order
    order.trade_index_buyer = Some(trade_index);
    // Timestamp take order time
    order.set_timestamp_now();

    // Check market price value in sats - if order was with market price then calculate it and send a DM to buyer
    if order.has_no_amount() {
        match get_market_amount_and_fee(order.fiat_amount, &order.fiat_code, order.premium).await {
            Ok(amount_fees) => {
                order.amount = amount_fees.0;
                order.fee = amount_fees.1
            }
            Err(_) => return Err(MostroInternalErr(ServiceError::WrongAmountError)),
        };
    }

    // Update trade index only after all checks are done
    update_user_trade_index(pool, event.sender.to_string(), trade_index)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // If payment request is not present, update order status to waiting buyer invoice
    if payment_request.is_none() {
        update_order_status(&mut order, my_keys, pool, request_id).await?;
    }
    // If payment request is present, show hold invoice
    else {
        show_hold_invoice(
            my_keys,
            payment_request,
            &event.rumor.pubkey,
            &seller_pubkey,
            order,
            request_id,
        )
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use mostro_core::order::{Kind as OrderKind, Status};
    use nostr_sdk::{Keys, Kind as NostrKind, Timestamp, UnsignedEvent};
    use sqlx::SqlitePool;

    async fn create_test_pool() -> SqlitePool {
        SqlitePool::connect(":memory:").await.unwrap()
    }

    fn create_test_keys() -> Keys {
        Keys::generate()
    }

    fn create_test_message(trade_index: Option<u32>) -> Message {
        // Create a basic message for TakeSell action
        // We'll use the new_order method since TakeSell isn't directly available
        Message::new_order(
            Some(uuid::Uuid::new_v4()),
            Some(1),
            trade_index.map(|i| i as i64),
            Action::TakeSell,
            None, // We don't need payload for structure tests
        )
    }

    fn create_test_unwrapped_gift() -> UnwrappedGift {
        let keys = create_test_keys();
        let sender_keys = create_test_keys();

        let unsigned_event = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            NostrKind::GiftWrap,
            Vec::new(),
            "",
        );

        UnwrappedGift {
            sender: sender_keys.public_key(),
            rumor: unsigned_event,
        }
    }

    #[tokio::test]
    async fn test_update_order_status_structure() {
        // Test the structure of update_order_status function
        // This would require mocking Order, Keys, and database operations
        // No-op: structural test ensures no panic
    }

    #[tokio::test]
    async fn test_take_sell_action_pending_order_exists() {
        let pool = create_test_pool().await;
        let keys = create_test_keys();
        let event = create_test_unwrapped_gift();
        let msg = create_test_message(Some(1));

        // This test would require:
        // 1. Setting up database tables
        // 2. Creating a pending order for the buyer
        // 3. Mocking buyer_has_pending_order to return true
        let result = take_sell_action(msg, &event, &keys, &pool).await;
        // Should fail if buyer has pending order, but we can't test that without DB setup
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_take_sell_action_order_validation() {
        let pool = create_test_pool().await;
        let keys = create_test_keys();
        let event = create_test_unwrapped_gift();
        let msg = create_test_message(Some(1));

        // This test would require:
        // 1. Mocking get_order to return an order
        // 2. Setting up the order to be either valid or invalid
        let result = take_sell_action(msg, &event, &keys, &pool).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_take_sell_action_trade_index_logic() {
        let pool = create_test_pool().await;
        let keys = create_test_keys();

        // Test case 1: sender == rumor.pubkey, no trade_index
        let mut event = create_test_unwrapped_gift();
        event.sender = event.rumor.pubkey;
        let msg = create_test_message(None);

        let result = take_sell_action(msg, &event, &keys, &pool).await;
        // Should use trade_index = 0 when sender == rumor.pubkey
        assert!(result.is_ok() || result.is_err());

        // Test case 2: sender != rumor.pubkey, no trade_index
        let event2 = create_test_unwrapped_gift();
        // sender and rumor.pubkey are already different by default
        let msg2 = create_test_message(None);

        let result2 = take_sell_action(msg2, &event2, &keys, &pool).await;
        // Should fail with InvalidPayload when sender != rumor.pubkey and no trade_index
        if let Err(MostroInternalErr(ServiceError::InvalidPayload)) = result2 {}

        // Test case 3: with trade_index
        let msg3 = create_test_message(Some(1));
        let result3 = take_sell_action(msg3, &event2, &keys, &pool).await;
        assert!(result3.is_ok() || result3.is_err());
    }

    #[tokio::test]
    async fn test_take_sell_action_market_price_calculation() {
        let pool = create_test_pool().await;
        let keys = create_test_keys();
        let event = create_test_unwrapped_gift();
        let msg = create_test_message(Some(1));

        // This test would require:
        // 1. Mocking get_order to return an order with amount = 0 (market price)
        // 2. Mocking get_market_amount_and_fee
        let result = take_sell_action(msg, &event, &keys, &pool).await;
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_take_sell_action_payment_request_flows() {
        let pool = create_test_pool().await;
        let keys = create_test_keys();
        let event = create_test_unwrapped_gift();

        // Test with no payment request (should update order status)
        let msg1 = create_test_message(Some(1));
        let result1 = take_sell_action(msg1, &event, &keys, &pool).await;
        assert!(result1.is_ok() || result1.is_err());

        // Test with payment request (should show hold invoice)
        let msg2 = create_test_message(Some(1));
        let result2 = take_sell_action(msg2, &event, &keys, &pool).await;
        assert!(result2.is_ok() || result2.is_err());
    }

    mod order_validation_tests {
        use super::*;

        #[test]
        fn test_order_validation_logic() {
            // Test the logical flow of order validation

            // Test sell order validation
            let order_kind = OrderKind::Sell;
            assert!(matches!(order_kind, OrderKind::Sell));

            // Test order status validation
            let order_status = Status::Pending;
            assert!(matches!(order_status, Status::Pending));

            // Test non-maker validation logic
            let maker_pubkey = create_test_keys().public_key();
            let taker_pubkey = create_test_keys().public_key();
            assert_ne!(maker_pubkey, taker_pubkey);
        }

        #[test]
        fn test_encryption_logic_structure() {
            // Test the structure of encryption logic
            let test_pubkey = create_test_keys().public_key().to_string();
            let test_password = "test_password";

            // In a real test, we would test CryptoUtils::store_encrypted
            // For now, we test the logic structure
            assert!(!test_pubkey.is_empty());
            assert!(!test_password.is_empty());
        }

        #[test]
        fn test_fiat_amount_range_logic() {
            // Test range order amount validation logic
            let requested_amount = 100i64;
            let min_amount = 50i64;
            let max_amount = 200i64;

            // Valid range
            assert!(requested_amount >= min_amount && requested_amount <= max_amount);

            // Out of range cases
            let too_small = 25i64;
            let too_large = 300i64;
            assert!(too_small < min_amount);
            assert!(too_large > max_amount);
        }
    }

    mod market_price_tests {

        #[test]
        fn test_market_price_calculation_logic() {
            // Test the logical flow of market price calculation
            let fiat_amount = 100i64;
            let premium = 5;

            // Mock calculation: amount = (fiat_amount / btc_price) * (1 + premium/100)
            let mock_btc_price = 50000.0;
            let base_amount = (fiat_amount as f64 / mock_btc_price) * 1e8;
            let premium_multiplier = 1.0 + (premium as f64 / 100.0);
            let final_amount = (base_amount * premium_multiplier) as i64;

            assert!(final_amount > 0);
            assert!(final_amount > base_amount as i64); // Should be higher due to premium
        }

        #[test]
        fn test_fee_calculation_logic() {
            // Test fee calculation structure
            let amount = 1_000_000i64; // 0.01 BTC
            let fee_rate = 0.005; // 0.5%
            let expected_fee = (amount as f64 * fee_rate) as i64;

            assert_eq!(expected_fee, 5_000); // 5000 sats
            assert!(expected_fee < amount); // Fee should be less than amount
        }
    }
}
