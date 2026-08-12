use crate::app::context::AppContext;
use crate::db::{claim_order_rating_flag, update_user_rating};
use crate::util::{enqueue_order_msg, get_order, update_user_rating_event};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

pub fn prepare_variables_for_vote(
    message_sender: &str,
    order: &Order,
) -> Result<(String, bool, bool), MostroError> {
    let mut counterpart_trade_pubkey: String = String::new();
    let mut buyer_rating: bool = false;
    let mut seller_rating: bool = false;

    // Get needed info about users
    let (seller, buyer) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (seller.to_owned(), buyer.to_owned()),
        (None, _) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
        (_, None) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };

    // Find the counterpart public key
    if message_sender == buyer {
        buyer_rating = true;
        counterpart_trade_pubkey = order
            .get_buyer_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
    } else if message_sender == seller {
        seller_rating = true;
        counterpart_trade_pubkey = order
            .get_seller_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
    };

    Ok((counterpart_trade_pubkey, buyer_rating, seller_rating))
}

/// Updates a user's reputation based on a rating received from a trade counterpart.
///
/// This function handles the reputation update process for users after a successful trade.
/// It processes ratings from either the buyer or seller of a completed order and updates
/// the recipient's reputation metrics accordingly. The function also handles privacy mode
/// checks and ensures users can only rate their trade counterpart once.
///
/// # Arguments
///
/// * `ctx` - Application context containing the database pool and other dependencies
/// * `msg` - The message containing the rating information
/// * `event` - The unwrapped gift event containing the sender's information
/// * `my_keys` - The keys used for signing events
///
/// # Returns
///
/// * `Result<(), MostroError>` - Returns `Ok(())` if the reputation update was successful,
///   or an appropriate error if something went wrong during the process.
///
/// # Process Flow
///
/// 1. Retrieves the order information from the database
/// 2. Verifies the order status is "Success", or "SettledHoldInvoice" for seller-initiated ratings
/// 3. Determines if the rating is from buyer or seller
/// 4. Fast-path skips when the sender's rate flag is already set (durable claim is step 6)
/// 5. Validates privacy mode settings
/// 6. Claims the sender's rating flag and updates the recipient's metrics in one transaction
/// 7. Creates and enqueues a new rating event after commit
/// 8. Sends a confirmation message to the rating user
pub async fn update_user_reputation_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get order
    let order = get_order(&msg, pool).await?;

    // Prepare variables for vote
    let (counterpart_trade_pubkey, buyer_rating, seller_rating) =
        prepare_variables_for_vote(&event.sender.to_string(), &order)?;

    // Check if order is success, but sellers can rate in status settled-hold-invoice
    if !(order.check_status(Status::Success).is_ok()
        || (order.check_status(Status::SettledHoldInvoice).is_ok() && seller_rating))
    {
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

    // Check if the order is not rated by the message sender
    // Check what rate status needs update
    let mut update_seller_rate = false;
    let mut update_buyer_rate = false;
    if seller_rating && !order.seller_sent_rate {
        update_seller_rate = true;
    } else if buyer_rating && !order.buyer_sent_rate {
        update_buyer_rate = true;
    };
    if !update_buyer_rate && !update_seller_rate {
        return Ok(());
    };

    // Get rating from message
    let new_rating = msg
        .get_inner_message_kind()
        .get_rating()
        .map_err(MostroInternalErr)?;

    // Check if users are in full privacy mode
    let (normal_buyer_idkey, normal_seller_idkey) = order
        .is_full_privacy_order()
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;

    // Resolve which identity key receives the vote (skip full-privacy counterpart)
    let rated_pubkey = if buyer_rating {
        match normal_seller_idkey {
            Some(seller_key) => seller_key,
            None => return Ok(()),
        }
    } else {
        match normal_buyer_idkey {
            Some(buyer_key) => buyer_key,
            None => return Ok(()),
        }
    };

    // Claim the order-side flag and apply the aggregate in one transaction so a
    // concurrent Rate cannot double-apply, and so a lost claim never mutates
    // the users table. Only the applicable unset flag is written.
    let mut tx = pool.begin().await.map_err(|e| {
        MostroInternalErr(ServiceError::DbAccessError(format!(
            "Failed to begin rating transaction: {e}"
        )))
    })?;

    let claimed = claim_order_rating_flag(&mut tx, order.id, update_buyer_rate).await?;
    if !claimed {
        // Another writer won the flag (or status left the eligible window).
        return Ok(());
    }

    // Read the rated user inside the transaction for a consistent aggregate base.
    let mut user_to_vote = sqlx::query_as::<_, User>(
        r#"
            SELECT *
            FROM users
            WHERE pubkey == ?1
            LIMIT 1
        "#,
    )
    .bind(&rated_pubkey)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    user_to_vote.update_rating(new_rating);

    update_user_rating(
        &mut *tx,
        user_to_vote.pubkey.clone(),
        user_to_vote.last_rating,
        user_to_vote.min_rating,
        user_to_vote.max_rating,
        user_to_vote.total_reviews,
        user_to_vote.total_rating,
    )
    .await?;

    tx.commit().await.map_err(|e| {
        MostroInternalErr(ServiceError::DbAccessError(format!(
            "Failed to commit rating transaction: {e}"
        )))
    })?;

    // Create new rating event only after the claim+aggregate commit.
    let reputation_event = Rating::new(
        user_to_vote.total_reviews as u64,
        user_to_vote.total_rating as f64,
        user_to_vote.last_rating as u8,
        user_to_vote.min_rating as u8,
        user_to_vote.max_rating as u8,
    )
    .to_tags();

    let days = calculate_days_since_creation(user_to_vote.created_at);
    let mut tags: Vec<Tag> = reputation_event.into_iter().collect();
    tags.push(Tag::custom("days", vec![days.to_string()]));
    let reputation_event = Tags::from_list(tags);

    if buyer_rating || seller_rating {
        update_user_rating_event(&counterpart_trade_pubkey, reputation_event, my_keys).await?;

        enqueue_order_msg(
            msg.get_inner_message_kind().request_id,
            Some(order.id),
            Action::RateReceived,
            Some(Payload::RatingUser(new_rating)),
            event.sender,
            None,
        )
        .await;
    }

    Ok(())
}

/// Calculate the number of days since user creation.
fn calculate_days_since_creation(created_at: i64) -> u64 {
    const SECONDS_IN_DAY: u64 = 86_400;
    let now = Timestamp::now().as_secs();
    u64::try_from(created_at)
        .ok()
        .filter(|ts| *ts > 0)
        .map(|ts| now.saturating_sub(ts) / SECONDS_IN_DAY)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::Settings;
    use crate::config::MOSTRO_CONFIG;
    use mostro_core::db::Crud;
    use mostro_core::message::{MessageKind, Payload};
    use mostro_core::order::Order;
    use nostr_sdk::prelude::{Keys, Timestamp};
    use sqlx::SqlitePool;
    use uuid::Uuid;

    fn init_test_settings() {
        crate::config::init_test_nostr_keys();
        let _ = MOSTRO_CONFIG.set(Settings {
            database: Default::default(),
            nostr: crate::config::NostrSettings {
                // Valid canonical test nsec: whichever module wins the
                // MOSTRO_CONFIG race must install a parseable key, or tests
                // that reach get_keys() flake on init ordering.
                nsec_privkey: secrecy::SecretString::from(
                    "nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd",
                ),
                relays: vec![],
            },
            mostro: Default::default(),
            lightning: Default::default(),
            rpc: Default::default(),
            expiration: Some(Default::default()),
            anti_abuse_bond: None,
            cashu: None,
            price: None,
        });
    }

    async fn create_test_pool() -> SqlitePool {
        init_test_settings();
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    fn create_test_keys() -> Keys {
        Keys::generate()
    }

    /// Build an `UnwrappedMessage` whose trade key (rumor author / `sender`)
    /// is `pubkey` — these tests gate on `event.sender` to identify
    /// buyer/seller — and whose identity key is generated separately so the
    /// fixture exercises the dual-key flow rather than full-privacy mode.
    fn create_unwrapped_message_with_pubkey(pubkey: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::Order(MessageKind::new(
                Some(Uuid::new_v4()),
                Some(1),
                None,
                Action::RateUser,
                None,
            )),
            signature: None,
            sender: pubkey,
            identity: Keys::generate().public_key(),
            created_at: Timestamp::now(),
        }
    }

    fn create_rate_user_message(order_id: Uuid, rating: u8) -> Message {
        let kind = MessageKind::new(
            Some(order_id),
            Some(1),
            None,
            Action::RateUser,
            Some(Payload::RatingUser(rating)),
        );
        Message::Order(kind)
    }

    fn create_test_order(
        status: Status,
        seller_pubkey: PublicKey,
        buyer_pubkey: PublicKey,
    ) -> Order {
        Order {
            id: Uuid::new_v4(),
            status: status.to_string(),
            seller_pubkey: Some(seller_pubkey.to_string()),
            buyer_pubkey: Some(buyer_pubkey.to_string()),
            seller_sent_rate: false,
            buyer_sent_rate: false,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_update_user_reputation_allows_success_status() {
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();

        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let seller_pk = seller_keys.public_key();
        let buyer_pk = buyer_keys.public_key();

        // Event where the sender is the seller (so seller_rating = true)
        let event = create_unwrapped_message_with_pubkey(seller_pk);

        // Insert Success order in DB
        let order = create_test_order(Status::Success, seller_pk, buyer_pk);
        let order = order.create(&pool).await.unwrap();

        // Message pointing to that order id with a valid rating payload
        let msg = create_rate_user_message(order.id, 5);

        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;

        // A Success order must not be rejected with InvalidOrderStatus
        if let Err(MostroCantDo(CantDoReason::InvalidOrderStatus)) = result {
            panic!("valid Success status must not be rejected");
        }
    }

    #[tokio::test]
    async fn test_update_user_reputation_rejects_settled_hold_invoice_buyer() {
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();

        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let seller_pk = seller_keys.public_key();
        let buyer_pk = buyer_keys.public_key();

        // Event where the sender is the buyer (so buyer_rating = true)
        let event = create_unwrapped_message_with_pubkey(buyer_pk);

        // SettledHoldInvoice order in DB
        let order = create_test_order(Status::SettledHoldInvoice, seller_pk, buyer_pk);
        let order = order.create(&pool).await.unwrap();

        let msg = create_rate_user_message(order.id, 5);

        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;

        // Buyer must not be allowed to rate in SettledHoldInvoice status
        match result {
            Err(MostroCantDo(CantDoReason::InvalidOrderStatus)) => {}
            _ => panic!("buyer should not be able to rate SettledHoldInvoice order"),
        }
    }

    #[tokio::test]
    async fn test_update_user_reputation_updates_buyer_and_order_flags() {
        use crate::db::{add_new_user, is_user_present};

        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();

        // Trade keys (ephemeral per-trade)
        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let seller_pk = seller_keys.public_key();
        let buyer_pk = buyer_keys.public_key();

        // Identity keys (master keys, must differ from trade keys)
        let seller_id_keys = create_test_keys();
        let buyer_id_keys = create_test_keys();
        let seller_id = seller_id_keys.public_key().to_string();
        let buyer_id = buyer_id_keys.public_key().to_string();

        // Counterpart user (seller identity) exists in DB so rating can be applied
        let seller_user = User {
            pubkey: seller_id.clone(),
            ..Default::default()
        };
        add_new_user(&pool, seller_user).await.unwrap();

        // Success order with master keys set (not full-privacy)
        let mut order = create_test_order(Status::Success, seller_pk, buyer_pk);
        order.master_seller_pubkey = Some(seller_id.clone());
        order.master_buyer_pubkey = Some(buyer_id.clone());
        let order = order.create(&pool).await.unwrap();

        // Event where sender is the buyer (buyer_rating = true)
        let event = create_unwrapped_message_with_pubkey(buyer_pk);
        let msg = create_rate_user_message(order.id, 5);

        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;
        assert!(result.is_ok());

        // The seller (counterpart of buyer rating) must have updated reputation
        let seller_user = is_user_present(&pool, seller_id).await.unwrap();
        assert_eq!(seller_user.total_reviews, 1);
        assert_eq!(seller_user.last_rating, 5);
        assert_eq!(seller_user.min_rating, 5);
        assert_eq!(seller_user.max_rating, 5);
        // First vote uses weight 1/2: total_rating = rating / 2.0
        assert!((seller_user.total_rating - 2.5).abs() < f64::EPSILON);

        // Order buyer_sent_rate flag must be set via the rating claim CAS
        let updated_order = Order::by_id(&pool, order.id)
            .await
            .unwrap()
            .expect("order not found");
        assert!(updated_order.buyer_sent_rate);
    }

    #[tokio::test]
    async fn test_update_user_reputation_buyer_already_rated_is_noop() {
        use crate::db::{add_new_user, is_user_present};

        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();

        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let seller_pk = seller_keys.public_key();
        let buyer_pk = buyer_keys.public_key();

        let seller_id_keys = create_test_keys();
        let buyer_id_keys = create_test_keys();
        let seller_id = seller_id_keys.public_key().to_string();
        let buyer_id = buyer_id_keys.public_key().to_string();

        let seller_user = User {
            pubkey: seller_id.clone(),
            ..Default::default()
        };
        add_new_user(&pool, seller_user).await.unwrap();

        // Order where buyer has already rated
        let mut order = create_test_order(Status::Success, seller_pk, buyer_pk);
        order.master_seller_pubkey = Some(seller_id.clone());
        order.master_buyer_pubkey = Some(buyer_id.clone());
        order.buyer_sent_rate = true;
        let order = order.create(&pool).await.unwrap();

        // Buyer tries to rate again
        let event = create_unwrapped_message_with_pubkey(buyer_pk);
        let msg = create_rate_user_message(order.id, 5);

        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;
        assert!(result.is_ok());

        // Seller reputation must remain unchanged (no double-rating)
        let seller_user = is_user_present(&pool, seller_id).await.unwrap();
        assert_eq!(seller_user.total_reviews, 0);
        assert!((seller_user.total_rating - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_update_user_reputation_updates_seller_and_order_flags() {
        use crate::db::{add_new_user, is_user_present};

        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();

        // Trade keys (ephemeral per-trade)
        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let seller_pk = seller_keys.public_key();
        let buyer_pk = buyer_keys.public_key();

        // Identity keys (master keys, must differ from trade keys)
        let seller_id_keys = create_test_keys();
        let buyer_id_keys = create_test_keys();
        let seller_id = seller_id_keys.public_key().to_string();
        let buyer_id = buyer_id_keys.public_key().to_string();

        // Counterpart user (buyer identity) exists in DB so rating can be applied
        let buyer_user = User {
            pubkey: buyer_id.clone(),
            ..Default::default()
        };
        add_new_user(&pool, buyer_user).await.unwrap();

        // Success order with master keys set (not full-privacy)
        let mut order = create_test_order(Status::Success, seller_pk, buyer_pk);
        order.master_seller_pubkey = Some(seller_id.clone());
        order.master_buyer_pubkey = Some(buyer_id.clone());
        let order = order.create(&pool).await.unwrap();

        // Event where sender is the seller (seller_rating = true)
        let event = create_unwrapped_message_with_pubkey(seller_pk);
        let msg = create_rate_user_message(order.id, 4);

        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;
        assert!(result.is_ok());

        // The buyer (counterpart of seller rating) must have updated reputation
        let buyer_user = is_user_present(&pool, buyer_id).await.unwrap();
        assert_eq!(buyer_user.total_reviews, 1);
        assert_eq!(buyer_user.last_rating, 4);
        assert_eq!(buyer_user.min_rating, 4);
        assert_eq!(buyer_user.max_rating, 4);
        // First vote uses weight 1/2: total_rating = rating / 2.0
        assert!((buyer_user.total_rating - 2.0).abs() < f64::EPSILON);

        // Order seller_sent_rate flag must be set via the rating claim CAS
        let updated_order = Order::by_id(&pool, order.id)
            .await
            .unwrap()
            .expect("order not found");
        assert!(updated_order.seller_sent_rate);
    }

    #[test]
    fn test_prepare_variables_for_vote_buyer() {
        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let order = create_test_order(
            Status::Success,
            seller_keys.public_key(),
            buyer_keys.public_key(),
        );

        let result = prepare_variables_for_vote(&buyer_keys.public_key().to_string(), &order);

        assert!(result.is_ok());
        let (_, buyer_rating, seller_rating) = result.unwrap();
        assert!(buyer_rating);
        assert!(!seller_rating);
    }

    #[test]
    fn test_prepare_variables_for_vote_seller() {
        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let order = create_test_order(
            Status::Success,
            seller_keys.public_key(),
            buyer_keys.public_key(),
        );

        let result = prepare_variables_for_vote(&seller_keys.public_key().to_string(), &order);

        assert!(result.is_ok());
        let (_, buyer_rating, seller_rating) = result.unwrap();
        assert!(!buyer_rating);
        assert!(seller_rating);
    }

    #[test]
    fn test_rating_validation_success_status() {
        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let order = create_test_order(
            Status::Success,
            seller_keys.public_key(),
            buyer_keys.public_key(),
        );

        // Both buyer and seller should be able to rate in Success status
        assert!(order.check_status(Status::Success).is_ok());

        // Test seller rating validation
        let (_, _, seller_rating) =
            prepare_variables_for_vote(&seller_keys.public_key().to_string(), &order).unwrap();
        let can_rate_seller = order.check_status(Status::Success).is_ok()
            || (order.check_status(Status::SettledHoldInvoice).is_ok() && seller_rating);
        assert!(can_rate_seller);

        // Test buyer rating validation
        let (_, buyer_rating, _) =
            prepare_variables_for_vote(&buyer_keys.public_key().to_string(), &order).unwrap();
        let can_rate_buyer = order.check_status(Status::Success).is_ok()
            || (order.check_status(Status::SettledHoldInvoice).is_ok() && !buyer_rating);
        assert!(can_rate_buyer);
    }

    #[test]
    fn test_rating_validation_settled_hold_invoice_seller() {
        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let order = create_test_order(
            Status::SettledHoldInvoice,
            seller_keys.public_key(),
            buyer_keys.public_key(),
        );

        // Seller should be able to rate in SettledHoldInvoice status
        let (_, _, seller_rating) =
            prepare_variables_for_vote(&seller_keys.public_key().to_string(), &order).unwrap();
        let can_rate_seller = order.check_status(Status::Success).is_ok()
            || (order.check_status(Status::SettledHoldInvoice).is_ok() && seller_rating);
        assert!(can_rate_seller);
    }

    #[test]
    fn test_rating_validation_settled_hold_invoice_buyer_denied() {
        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let order = create_test_order(
            Status::SettledHoldInvoice,
            seller_keys.public_key(),
            buyer_keys.public_key(),
        );

        // Buyer should NOT be able to rate in SettledHoldInvoice status
        let (_, buyer_rating, _) =
            prepare_variables_for_vote(&buyer_keys.public_key().to_string(), &order).unwrap();
        let can_rate_buyer = order.check_status(Status::Success).is_ok()
            || (order.check_status(Status::SettledHoldInvoice).is_ok() && !buyer_rating);
        assert!(!can_rate_buyer);
    }

    #[test]
    fn test_rating_validation_invalid_status() {
        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let order = create_test_order(
            Status::Pending,
            seller_keys.public_key(),
            buyer_keys.public_key(),
        );

        // Neither buyer nor seller should be able to rate in Pending status
        let (_, buyer_rating, seller_rating) =
            prepare_variables_for_vote(&seller_keys.public_key().to_string(), &order).unwrap();

        let can_rate_seller = order.check_status(Status::Success).is_ok()
            || (order.check_status(Status::SettledHoldInvoice).is_ok() && seller_rating);
        assert!(!can_rate_seller);

        let can_rate_buyer = order.check_status(Status::Success).is_ok()
            || (order.check_status(Status::SettledHoldInvoice).is_ok() && !buyer_rating);
        assert!(!can_rate_buyer);
    }

    #[test]
    fn test_calculate_days_since_creation_normal() {
        let now = Timestamp::now().as_secs();
        // User created 10 days ago
        let created_at = (now - 10 * 86_400) as i64;
        let days = calculate_days_since_creation(created_at);
        assert_eq!(days, 10);
    }

    #[test]
    fn test_calculate_days_since_creation_zero() {
        // New user with created_at = 0 should return 0 days
        let days = calculate_days_since_creation(0);
        assert_eq!(days, 0);
    }

    #[test]
    fn test_calculate_days_since_creation_negative() {
        // Corrupted created_at should return 0 days
        let days = calculate_days_since_creation(-1);
        assert_eq!(days, 0);
    }

    #[test]
    fn test_calculate_days_since_creation_partial_day() {
        let now = Timestamp::now().as_secs();
        // Created 1.5 days ago - should truncate to 1
        let created_at = (now - 86_400 - 43_200) as i64;
        let days = calculate_days_since_creation(created_at);
        assert_eq!(days, 1);
    }

    #[test]
    fn test_prepare_variables_missing_seller_pubkey_errors() {
        let buyer_keys = create_test_keys();
        let mut order = create_test_order(
            Status::Success,
            create_test_keys().public_key(),
            buyer_keys.public_key(),
        );
        order.seller_pubkey = None;
        let result = prepare_variables_for_vote(&buyer_keys.public_key().to_string(), &order);
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
    }

    #[test]
    fn test_prepare_variables_missing_buyer_pubkey_errors() {
        let seller_keys = create_test_keys();
        let mut order = create_test_order(
            Status::Success,
            seller_keys.public_key(),
            create_test_keys().public_key(),
        );
        order.buyer_pubkey = None;
        let result = prepare_variables_for_vote(&seller_keys.public_key().to_string(), &order);
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
    }

    #[test]
    fn test_prepare_variables_unknown_sender_sets_no_flags() {
        // A sender that is neither buyer nor seller falls through both
        // arms: no rating flag set and an empty counterpart pubkey.
        let order = create_test_order(
            Status::Success,
            create_test_keys().public_key(),
            create_test_keys().public_key(),
        );
        let stranger = create_test_keys().public_key().to_string();
        let (counterpart, buyer_rating, seller_rating) =
            prepare_variables_for_vote(&stranger, &order).unwrap();
        assert!(counterpart.is_empty());
        assert!(!buyer_rating);
        assert!(!seller_rating);
    }

    #[tokio::test]
    async fn test_buyer_rating_full_privacy_seller_is_silent_noop() {
        // Buyer rates, but the seller has no distinct identity key
        // (full privacy): there is no user row to credit, so the action
        // returns Ok without touching anything.
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let seller_pk = create_test_keys().public_key();
        let buyer_pk = create_test_keys().public_key();

        // No master_seller_pubkey → seller reads as full-privacy.
        let mut order = create_test_order(Status::Success, seller_pk, buyer_pk);
        order.master_buyer_pubkey = Some(create_test_keys().public_key().to_string());
        let order = order.create(&pool).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(buyer_pk);
        let msg = create_rate_user_message(order.id, 5);
        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_seller_rating_full_privacy_buyer_is_silent_noop() {
        // Mirror case: seller rates a full-privacy buyer → Ok, no-op.
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let seller_pk = create_test_keys().public_key();
        let buyer_pk = create_test_keys().public_key();

        let mut order = create_test_order(Status::Success, seller_pk, buyer_pk);
        order.master_seller_pubkey = Some(create_test_keys().public_key().to_string());
        let order = order.create(&pool).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(seller_pk);
        let msg = create_rate_user_message(order.id, 5);
        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_buyer_rating_with_absent_seller_user_row_errors() {
        // Seller identity key set, but no matching row in `users`:
        // `is_user_present` fails and must surface as DbAccessError.
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let seller_pk = create_test_keys().public_key();
        let buyer_pk = create_test_keys().public_key();

        let mut order = create_test_order(Status::Success, seller_pk, buyer_pk);
        order.master_seller_pubkey = Some(create_test_keys().public_key().to_string());
        order.master_buyer_pubkey = Some(create_test_keys().public_key().to_string());
        let order = order.create(&pool).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(buyer_pk);
        let msg = create_rate_user_message(order.id, 5);
        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::DbAccessError(_)))
        ));
    }

    #[tokio::test]
    async fn test_seller_rating_with_absent_buyer_user_row_errors() {
        // Mirror case for the seller-rating arm of the privacy lookup.
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let seller_pk = create_test_keys().public_key();
        let buyer_pk = create_test_keys().public_key();

        let mut order = create_test_order(Status::Success, seller_pk, buyer_pk);
        order.master_seller_pubkey = Some(create_test_keys().public_key().to_string());
        order.master_buyer_pubkey = Some(create_test_keys().public_key().to_string());
        let order = order.create(&pool).await.unwrap();

        let event = create_unwrapped_message_with_pubkey(seller_pk);
        let msg = create_rate_user_message(order.id, 5);
        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::DbAccessError(_)))
        ));
    }

    #[tokio::test]
    async fn test_update_user_rating_db_failure_surfaces_error() {
        // Force the `users` UPDATE to fail via a RAISE trigger so the
        // `update_user_rating` error branch is exercised without
        // touching production code.
        use crate::db::add_new_user;

        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let seller_pk = create_test_keys().public_key();
        let buyer_pk = create_test_keys().public_key();
        let seller_id = create_test_keys().public_key().to_string();

        let seller_user = User {
            pubkey: seller_id.clone(),
            ..Default::default()
        };
        add_new_user(&pool, seller_user).await.unwrap();

        let mut order = create_test_order(Status::Success, seller_pk, buyer_pk);
        order.master_seller_pubkey = Some(seller_id.clone());
        order.master_buyer_pubkey = Some(create_test_keys().public_key().to_string());
        let order = order.create(&pool).await.unwrap();

        sqlx::query(
            "CREATE TRIGGER fail_users_update BEFORE UPDATE ON users \
             BEGIN SELECT RAISE(ABORT, 'forced users failure'); END;",
        )
        .execute(&pool)
        .await
        .unwrap();

        let event = create_unwrapped_message_with_pubkey(buyer_pk);
        let msg = create_rate_user_message(order.id, 5);
        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::DbAccessError(_)))
        ));
    }

    #[tokio::test]
    async fn test_update_user_rating_event_db_failure_surfaces_error() {
        // The rating claim UPDATE on `orders` fails via a RAISE trigger —
        // the transaction must abort and surface DbAccessError (no aggregate
        // write, no queued rating event).
        use crate::db::add_new_user;

        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let seller_pk = create_test_keys().public_key();
        let buyer_pk = create_test_keys().public_key();
        let seller_id = create_test_keys().public_key().to_string();

        let seller_user = User {
            pubkey: seller_id.clone(),
            ..Default::default()
        };
        add_new_user(&pool, seller_user).await.unwrap();

        let mut order = create_test_order(Status::Success, seller_pk, buyer_pk);
        order.master_seller_pubkey = Some(seller_id.clone());
        order.master_buyer_pubkey = Some(create_test_keys().public_key().to_string());
        let order = order.create(&pool).await.unwrap();

        sqlx::query(
            "CREATE TRIGGER fail_orders_update BEFORE UPDATE ON orders \
             BEGIN SELECT RAISE(ABORT, 'forced orders failure'); END;",
        )
        .execute(&pool)
        .await
        .unwrap();

        let event = create_unwrapped_message_with_pubkey(buyer_pk);
        let msg = create_rate_user_message(order.id, 5);
        let result = update_user_reputation_action(&ctx, msg, &event, &keys).await;
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::DbAccessError(_)))
        ));
    }
}
