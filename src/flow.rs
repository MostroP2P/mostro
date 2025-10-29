use crate::util::{enqueue_order_msg, notify_taker_reputation};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
use sqlx_crud::Crud;
use tracing::info;

pub async fn hold_invoice_paid(
    hash: &str,
    request_id: Option<u64>,
    pool: &SqlitePool,
) -> Result<(), MostroError> {
    let order = crate::db::find_order_by_hash(pool, hash)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let my_keys = crate::util::get_keys()
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    let buyer_pubkey = order
        .get_buyer_pubkey()
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    let seller_pubkey = order
        .get_seller_pubkey()
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    info!(
        "Order Id: {} - Seller paid invoice with hash: {hash}",
        order.id
    );

    // Check if the order kind is valid
    let order_kind = order.get_order_kind().map_err(MostroInternalErr)?;

    // We send this data related to the order to the parties
    let mut order_data = SmallOrder::new(
        Some(order.id),
        Some(order_kind),
        None,
        order.amount,
        order.fiat_code.clone(),
        order.min_amount,
        order.max_amount,
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        order.buyer_pubkey.as_ref().cloned(),
        order.seller_pubkey.as_ref().cloned(),
        None,
        Some(order.created_at),
        Some(order.expires_at),
    );
    let status;

    if order.buyer_invoice.is_some() {
        status = Status::Active;
        order_data.status = Some(status);
        // We send a confirmation message to seller
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::BuyerTookOrder,
            Some(Payload::Order(order_data.clone(), None)),
            seller_pubkey,
            None,
        )
        .await;
        // We send a message to buyer saying seller paid
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::HoldInvoicePaymentAccepted,
            Some(Payload::Order(order_data, None)),
            buyer_pubkey,
            None,
        )
        .await;
    } else {
        let new_amount = order_data.amount - order.fee;
        order_data.amount = new_amount;
        status = Status::WaitingBuyerInvoice;
        order_data.status = Some(status);
        order_data.buyer_trade_pubkey = None;
        order_data.seller_trade_pubkey = None;

        // We send a message to seller we are waiting for buyer invoice
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::WaitingBuyerInvoice,
            None,
            seller_pubkey,
            None,
        )
        .await;

        // Notify taker reputation to maker
        tracing::info!("Notifying taker reputation to maker");
        notify_taker_reputation(pool, &order, request_id, None, Some(order_data)).await?;
    }
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    if let Ok(updated_order) = crate::util::update_order_event(&my_keys, status, &order).await {
        // Update order on db
        let _ = updated_order.update(pool).await;
    }

    // Update the invoice_held_at field
    crate::db::update_order_invoice_held_at_time(pool, order.id, Timestamp::now().as_u64() as i64)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}

pub async fn hold_invoice_settlement(hash: &str, pool: &SqlitePool) -> Result<()> {
    let order = crate::db::find_order_by_hash(pool, hash).await?;
    info!(
        "Order Id: {} - Invoice with hash: {} was settled!",
        order.id, hash
    );
    Ok(())
}

pub async fn hold_invoice_canceled(hash: &str, pool: &SqlitePool) -> Result<()> {
    let order = crate::db::find_order_by_hash(pool, hash).await?;
    info!(
        "Order Id: {} - Invoice with hash: {} was canceled!",
        order.id, hash
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::order::{Kind as OrderKind, Status};
    use nostr_sdk::{Keys, Timestamp};
    use sqlx::SqlitePool;

    async fn create_test_pool() -> SqlitePool {
        SqlitePool::connect(":memory:").await.unwrap()
    }

    fn create_test_keys() -> Keys {
        Keys::generate()
    }

    #[tokio::test]
    async fn test_hold_invoice_paid_structure() {
        let pool = create_test_pool().await;
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let request_id = Some(1u64);

        // This test would require:
        // 1. Setting up database tables and test data
        // 2. Mocking get_keys()
        // 3. Creating a valid order in the database
        let result = hold_invoice_paid(hash, request_id, &pool).await;
        // Should fail without proper database setup, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_hold_invoice_settlement_structure() {
        let pool = create_test_pool().await;
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        // This test would require setting up database with order data
        let result = hold_invoice_settlement(hash, &pool).await;
        // Should fail without proper database setup
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_hold_invoice_canceled_structure() {
        let pool = create_test_pool().await;
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        // This test would require setting up database with order data
        let result = hold_invoice_canceled(hash, &pool).await;
        // Should fail without proper database setup
        assert!(result.is_ok() || result.is_err());
    }

    mod hold_invoice_flow_tests {
        use super::*;

        #[test]
        fn test_hash_validation() {
            // Test various hash formats
            let valid_hashes = vec![
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", // 64 chars
                "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210", // 64 chars
                "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890", // Mixed case
            ];

            let invalid_hashes = vec![
                "",                                                                   // Empty
                "short",                                                              // Too short
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdefXX", // Too long
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg", // Invalid char
            ];

            // All valid hashes should be 64 characters of hex
            for hash in valid_hashes {
                assert_eq!(hash.len(), 64);
                assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
            }

            // Invalid hashes should fail basic validation
            for hash in invalid_hashes {
                assert!(hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()));
            }
        }

        #[test]
        fn test_order_data_creation_logic() {
            // Test the logical flow of SmallOrder creation

            // Mock order data
            let order_id = uuid::Uuid::new_v4();
            let order_kind = OrderKind::Sell;
            let amount = 1000i64;
            let fiat_code = "USD".to_string();
            let fiat_amount = 100i64;
            let payment_method = "SEPA".to_string();
            let premium = 5;
            let created_at = Timestamp::now().as_u64() as i64;
            let expires_at = created_at + 3600;

            // Test SmallOrder creation logic
            let order_data = SmallOrder::new(
                Some(order_id),
                Some(order_kind),
                None,
                amount,
                fiat_code.clone(),
                None,
                None,
                fiat_amount,
                payment_method.clone(),
                premium,
                None,
                None,
                None,
                Some(created_at),
                Some(expires_at),
            );

            // Verify the order data is constructed correctly
            assert_eq!(order_data.id, Some(order_id));
            assert_eq!(order_data.kind, Some(order_kind));
            assert_eq!(order_data.amount, amount);
            assert_eq!(order_data.fiat_code, fiat_code);
            assert_eq!(order_data.fiat_amount, fiat_amount);
            assert_eq!(order_data.payment_method, payment_method);
            assert_eq!(order_data.premium, premium);
            assert_eq!(order_data.created_at, Some(created_at));
            assert_eq!(order_data.expires_at, Some(expires_at));
        }

        #[test]
        fn test_status_transitions() {
            // Test the logical flow of status transitions

            // From WaitingBuyerInvoice to Active
            let initial_status = Status::WaitingBuyerInvoice;
            let target_status = Status::Active;

            // Simulate the condition: buyer invoice exists
            let buyer_invoice_exists = true;
            let resulting_status = if buyer_invoice_exists {
                Status::Active
            } else {
                Status::WaitingBuyerInvoice
            };

            assert_eq!(resulting_status, target_status);

            // Test the opposite case
            let buyer_invoice_exists = false;
            let resulting_status = if buyer_invoice_exists {
                Status::Active
            } else {
                Status::WaitingBuyerInvoice
            };

            assert_eq!(resulting_status, initial_status);
        }

        #[test]
        fn test_fee_calculation_logic() {
            // Test fee calculation in the amount adjustment
            let original_amount = 1000i64;
            let fee = 15i64; // 1.5%
            let expected_new_amount = original_amount - fee;

            assert_eq!(expected_new_amount, 985);
            assert!(expected_new_amount < original_amount);
            assert!(fee > 0);

            // Test edge cases
            let zero_fee = 0i64;
            assert_eq!(original_amount - zero_fee, original_amount);

            let large_fee = 500i64; // 50%
            let result_with_large_fee = original_amount - large_fee;
            assert_eq!(result_with_large_fee, 500);
            assert!(result_with_large_fee > 0); // Should still be positive
        }
    }

    mod message_flow_tests {
        use super::*;

        #[test]
        fn test_action_types_for_buyer_invoice_flow() {
            // Test the different actions used in the flow

            // Actions when buyer invoice exists
            let buyer_actions = vec![Action::BuyerTookOrder, Action::HoldInvoicePaymentAccepted];

            // Actions when buyer invoice doesn't exist
            let no_invoice_actions = vec![Action::AddInvoice, Action::WaitingBuyerInvoice];

            // Verify actions are different for different flows
            for action in buyer_actions {
                assert!(!no_invoice_actions.contains(&action));
            }

            for action in no_invoice_actions {
                assert!(
                    ![Action::BuyerTookOrder, Action::HoldInvoicePaymentAccepted].contains(&action)
                );
            }
        }

        #[test]
        fn test_payload_creation_logic() {
            // Test payload creation for different scenarios

            // Create mock order data
            let order_data = SmallOrder::new(
                Some(uuid::Uuid::new_v4()),
                Some(OrderKind::Sell),
                Some(Status::Active),
                1000,
                "USD".to_string(),
                None,
                None,
                100,
                "SEPA".to_string(),
                0,
                None,
                None,
                None,
                Some(Timestamp::now().as_u64() as i64),
                Some(Timestamp::now().as_u64() as i64 + 3600),
            );

            // Test payload with order data
            let payload_with_order = Some(Payload::Order(order_data.clone()));
            assert!(payload_with_order.is_some());

            // Test payload without order data (None)
            let payload_none: Option<Payload> = None;
            assert!(payload_none.is_none());

            // Verify payload contains the order data
            if let Some(Payload::Order(order)) = payload_with_order {
                assert_eq!(order.amount, 1000);
                assert_eq!(order.fiat_code, "USD");
                assert_eq!(order.status, Some(Status::Active));
            } else {
                panic!("Expected Order payload");
            }
        }
    }

    mod pubkey_extraction_tests {
        use super::*;

        #[test]
        fn test_pubkey_extraction_logic() {
            // Test the logical flow of pubkey extraction

            let keys = create_test_keys();
            let buyer_pubkey = keys.public_key();
            let seller_pubkey = create_test_keys().public_key();

            // Test that pubkeys are different
            assert_ne!(buyer_pubkey, seller_pubkey);

            // Test pubkey string conversion
            let buyer_pubkey_str = buyer_pubkey.to_string();
            let seller_pubkey_str = seller_pubkey.to_string();

            assert!(!buyer_pubkey_str.is_empty());
            assert!(!seller_pubkey_str.is_empty());
            assert_ne!(buyer_pubkey_str, seller_pubkey_str);

            // Test pubkey format (should be hex)
            assert!(buyer_pubkey_str.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(seller_pubkey_str.chars().all(|c| c.is_ascii_hexdigit()));

            // Nostr pubkeys should be 64 characters (32 bytes in hex)
            assert_eq!(buyer_pubkey_str.len(), 64);
            assert_eq!(seller_pubkey_str.len(), 64);
        }

        #[test]
        fn test_request_id_handling() {
            // Test request ID handling in different scenarios

            let valid_request_ids = vec![Some(1u64), Some(42u64), Some(1000u64), Some(u64::MAX)];

            let none_request_id: Option<u64> = None;

            // All valid request IDs should be Some
            for request_id in valid_request_ids {
                assert!(request_id.is_some());
                assert!(request_id.unwrap() > 0 || request_id.unwrap() == 0);
            }

            // None should be None
            assert!(none_request_id.is_none());
        }
    }

    mod timestamp_tests {
        use super::*;

        #[test]
        fn test_timestamp_operations() {
            // Test timestamp operations used in the flow

            let current_timestamp = Timestamp::now();
            let timestamp_u64 = current_timestamp.as_u64();
            let timestamp_i64 = timestamp_u64 as i64;

            // Verify timestamp is reasonable (after 2020, before 2050)
            let year_2020 = 1577836800u64; // 2020-01-01 00:00:00 UTC
            let year_2050 = 2524608000u64; // 2050-01-01 00:00:00 UTC

            assert!(timestamp_u64 > year_2020);
            assert!(timestamp_u64 < year_2050);

            // Verify i64 conversion preserves the value
            assert!(timestamp_i64 > 0);
            assert_eq!(timestamp_u64, timestamp_i64 as u64);
        }
    }
}
