use crate::app::context::AppContext;
use crate::db::{is_user_present, update_user_rating};
use crate::util::{enqueue_order_msg, get_order, update_user_rating_event};
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
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
/// * `msg` - The message containing the rating information
/// * `event` - The unwrapped gift event containing the sender's information
/// * `my_keys` - The keys used for signing events
/// * `pool` - The database connection pool
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
/// 4. Checks if the user has already rated their counterpart
/// 5. Validates privacy mode settings
/// 6. Updates the recipient's rating metrics
/// 7. Creates and saves a new rating event
/// 8. Updates the database with the new rating information
/// 9. Sends a confirmation message to the rating user
pub async fn update_user_reputation_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get order
    let order = get_order(&msg, pool).await?;

    // Prepare variables for vote
    let (counterpart_trade_pubkey, buyer_rating, seller_rating) =
        prepare_variables_for_vote(&event.rumor.pubkey.to_string(), &order)?;

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
        .is_full_privacy_order(None)
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;

    // Get counter to vote from db, but only if they're not in privacy mode
    let mut user_to_vote = if buyer_rating {
        // If buyer is rating seller, check if seller is in privacy mode
        if let Some(seller_key) = normal_seller_idkey {
            is_user_present(pool, seller_key).await.map_err(|cause| {
                MostroInternalErr(ServiceError::DbAccessError(cause.to_string()))
            })?
        } else {
            return Ok(());
        }
    } else {
        // If seller is rating buyer, check if buyer is in privacy mode
        if let Some(buyer_key) = normal_buyer_idkey {
            is_user_present(pool, buyer_key).await.map_err(|cause| {
                MostroInternalErr(ServiceError::DbAccessError(cause.to_string()))
            })?
        } else {
            return Ok(());
        }
    };

    // Calculate new rating
    user_to_vote.update_rating(new_rating);

    // Create new rating event
    let reputation_event = Rating::new(
        user_to_vote.total_reviews as u64,
        user_to_vote.total_rating as f64,
        user_to_vote.last_rating as u8,
        user_to_vote.min_rating as u8,
        user_to_vote.max_rating as u8,
    )
    .to_tags()
    .map_err(|cause| MostroInternalErr(ServiceError::NostrError(cause.to_string())))?;

    // Calculate days since user creation and add to rating tags
    let days = calculate_days_since_creation(user_to_vote.created_at);
    let mut tags: Vec<Tag> = reputation_event.into_iter().collect();
    tags.push(Tag::custom(
        TagKind::Custom(std::borrow::Cow::Borrowed("days")),
        vec![days.to_string()],
    ));
    let reputation_event = Tags::from_list(tags);

    // Save new rating to db
    if let Err(e) = update_user_rating(
        pool,
        user_to_vote.pubkey,
        user_to_vote.last_rating,
        user_to_vote.min_rating,
        user_to_vote.max_rating,
        user_to_vote.total_reviews,
        user_to_vote.total_rating,
    )
    .await
    {
        return Err(MostroInternalErr(ServiceError::DbAccessError(format!(
            "Error updating user rating : {}",
            e
        ))));
    }

    if buyer_rating || seller_rating {
        // Update db with rate flags
        update_user_rating_event(
            &counterpart_trade_pubkey,
            update_buyer_rate,
            update_seller_rate,
            reputation_event,
            &msg,
            my_keys,
            pool,
        )
        .await
        .map_err(|cause| {
            MostroInternalErr(ServiceError::DbAccessError(format!(
                "Error updating user rating event : {}",
                cause
            )))
        })?;

        // Send confirmation message to user that rated
        enqueue_order_msg(
            msg.get_inner_message_kind().request_id,
            Some(order.id),
            Action::RateReceived,
            Some(Payload::RatingUser(new_rating)),
            event.rumor.pubkey,
            None,
        )
        .await;
    }

    Ok(())
}

/// Calculate the number of days since user creation.
fn calculate_days_since_creation(created_at: i64) -> u64 {
    const SECONDS_IN_DAY: u64 = 86_400;
    let now = Timestamp::now().as_u64();
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
    use mostro_core::message::{MessageKind, Payload};
    use mostro_core::order::Order;
    use nostr_sdk::{Keys, Kind as NostrKind, Timestamp, UnsignedEvent};
    use sqlx::SqlitePool;
    use sqlx_crud::Crud;
    use uuid::Uuid;

    fn init_test_settings() {
        let _ = MOSTRO_CONFIG.set(Settings {
            database: Default::default(),
            nostr: Default::default(),
            mostro: Default::default(),
            lightning: Default::default(),
            rpc: Default::default(),
            expiration: Some(Default::default()),
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

    fn create_unwrapped_gift_with_pubkey(pubkey: PublicKey) -> UnwrappedGift {
        let unsigned_event = UnsignedEvent::new(
            pubkey,
            Timestamp::now(),
            NostrKind::GiftWrap,
            Vec::new(),
            "",
        );
        UnwrappedGift {
            sender: pubkey,
            rumor: unsigned_event,
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
        use crate::app::context::test_utils::{TestContextBuilder, test_settings};
        let ctx = TestContextBuilder::new().with_pool(std::sync::Arc::new(pool.clone())).with_settings(test_settings()).build();
        let keys = create_test_keys();

        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let seller_pk = seller_keys.public_key();
        let buyer_pk = buyer_keys.public_key();

        // Event where the sender is the seller (so seller_rating = true)
        let event = create_unwrapped_gift_with_pubkey(seller_pk);

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
        use crate::app::context::test_utils::{TestContextBuilder, test_settings};
        let ctx = TestContextBuilder::new().with_pool(std::sync::Arc::new(pool.clone())).with_settings(test_settings()).build();
        let keys = create_test_keys();

        let seller_keys = create_test_keys();
        let buyer_keys = create_test_keys();
        let seller_pk = seller_keys.public_key();
        let buyer_pk = buyer_keys.public_key();

        // Event where the sender is the buyer (so buyer_rating = true)
        let event = create_unwrapped_gift_with_pubkey(buyer_pk);

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
        use crate::app::context::test_utils::{TestContextBuilder, test_settings};
        let ctx = TestContextBuilder::new().with_pool(std::sync::Arc::new(pool.clone())).with_settings(test_settings()).build();
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
        let event = create_unwrapped_gift_with_pubkey(buyer_pk);
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

        // Order buyer_sent_rate flag must be set via update_user_rating_event
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
        use crate::app::context::test_utils::{TestContextBuilder, test_settings};
        let ctx = TestContextBuilder::new().with_pool(std::sync::Arc::new(pool.clone())).with_settings(test_settings()).build();
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
        let event = create_unwrapped_gift_with_pubkey(buyer_pk);
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
        use crate::app::context::test_utils::{TestContextBuilder, test_settings};
        let ctx = TestContextBuilder::new().with_pool(std::sync::Arc::new(pool.clone())).with_settings(test_settings()).build();
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
        let event = create_unwrapped_gift_with_pubkey(seller_pk);
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

        // Order seller_sent_rate flag must be set via update_user_rating_event
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
        let now = Timestamp::now().as_u64();
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
        let now = Timestamp::now().as_u64();
        // Created 1.5 days ago - should truncate to 1
        let created_at = (now - 86_400 - 43_200) as i64;
        let days = calculate_days_since_creation(created_at);
        assert_eq!(days, 1);
    }
}
