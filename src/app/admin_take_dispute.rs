use crate::app::admin_add_solver::SOLVER_CATEGORY_READ_ONLY;
use crate::app::context::AppContext;
use crate::db::{find_solver_pubkey, is_user_present, user_has_solver_write_permission};
use crate::nip33::{create_platform_tag_values, new_dispute_event};
use crate::util::{get_dispute, send_dm};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

use mostro_core::db::Crud;
use std::str::FromStr;
use tracing::info;

/// Prepares the solver information message for a dispute.
///
/// This asynchronous function checks the privacy status of the buyer and seller involved in a dispute,
/// retrieves their public keys if they are not in full privacy mode, and constructs a `SolverDisputeInfo`
/// object containing the necessary information for the solver to assist in the dispute resolution.
///
/// # Parameters
///
/// - `pool`: A reference to the database connection pool used to query user information.
/// - `order`: A reference to the `Order` object associated with the dispute, which contains details about the transaction.
/// - `dispute`: A reference to the `Dispute` object that holds the current state of the dispute.
///
/// # Returns
///
/// Returns a `Result<SolverDisputeInfo, MostroError>`. On success, it returns the constructed `SolverDisputeInfo`
/// object. On failure, it returns a `MostroError` indicating the reason for the failure, such as invalid public keys
/// or issues accessing the database.
///
/// # Errors
///
/// This function may return errors related to invalid public keys or database access issues, which are handled
/// by mapping them to `MostroError`.
async fn prepare_solver_info_message(
    pool: &Pool<Sqlite>,
    order: &Order,
    dispute: &Dispute,
) -> Result<SolverDisputeInfo, MostroError> {
    // Check if one or both users are in full privacy mode
    let (normal_buyer_idkey, normal_seller_idkey) = order
        .is_full_privacy_order()
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;

    // Get pubkeys of initiator and counterpart and users data if not in full privacy mode
    let buyer = if let Some(master_buyer_key) = normal_buyer_idkey {
        Some(is_user_present(pool, master_buyer_key).await?)
    } else {
        None
    };
    let seller = if let Some(master_seller_key) = normal_seller_idkey {
        Some(is_user_present(pool, master_seller_key).await?)
    } else {
        None
    };

    // Get disputes infos
    let (dispute_initiator, counterpart, initiator) =
        match (order.seller_dispute, order.buyer_dispute) {
            (true, false) => (
                order.get_seller_pubkey().map_err(MostroInternalErr)?,
                buyer,
                seller,
            ),
            (false, true) => (
                order.get_buyer_pubkey().map_err(MostroInternalErr)?,
                seller,
                buyer,
            ),
            (_, _) => return Err(MostroInternalErr(ServiceError::DisputeEventError)),
        };

    // Prepare dispute info
    let dispute_info = SolverDisputeInfo::new(
        order,
        dispute,
        dispute_initiator.to_string(),
        counterpart,
        initiator,
    );

    Ok(dispute_info)
}

pub async fn pubkey_event_can_solve(
    pool: &Pool<Sqlite>,
    ev_pubkey: &PublicKey,
    status: DisputeStatus,
    current_solver_pubkey: Option<&str>,
    my_keys: &Keys,
) -> bool {
    let sender_pubkey = ev_pubkey.to_string();

    // Is mostro admin taking dispute?
    info!(
        "admin pubkey {} -event pubkey {} ",
        my_keys.public_key().to_string(),
        sender_pubkey
    );
    if sender_pubkey == my_keys.public_key().to_string()
        && matches!(status, DisputeStatus::InProgress | DisputeStatus::Initiated)
    {
        return true;
    }

    // Sender must be a solver user
    let Ok(solver) = find_solver_pubkey(pool, sender_pubkey.clone()).await else {
        return false;
    };
    if solver.is_solver == 0_i64 {
        return false;
    }

    // Any solver can pick up a freshly initiated dispute
    if status == DisputeStatus::Initiated {
        return true;
    }

    // Takeover only applies to InProgress disputes
    if status != DisputeStatus::InProgress {
        return false;
    }

    // The currently assigned solver can always continue acting on the dispute
    let Some(current_solver_pubkey) = current_solver_pubkey else {
        return false;
    };
    if current_solver_pubkey == sender_pubkey {
        return true;
    }

    // Takeover path: a write-capable solver may take over from a read-only solver
    let sender_can_write = user_has_solver_write_permission(pool, sender_pubkey.as_str())
        .await
        .unwrap_or(false);
    if !sender_can_write {
        return false;
    }

    let Ok(current_solver) = find_solver_pubkey(pool, current_solver_pubkey.to_string()).await
    else {
        return false;
    };

    current_solver.is_solver != 0_i64 && current_solver.category == SOLVER_CATEGORY_READ_ONLY
}

pub async fn admin_take_dispute_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    mostro_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    // Get dispute
    let mut dispute = get_dispute(&msg, pool).await?;

    // Check if the pubkey is a solver or admin
    if let Ok(dispute_status) = DisputeStatus::from_str(&dispute.status) {
        if !pubkey_event_can_solve(
            pool,
            &event.identity,
            dispute_status,
            dispute.solver_pubkey.as_deref(),
            mostro_keys,
        )
        .await
        {
            // We create a Message
            return Err(MostroCantDo(CantDoReason::InvalidPubkey));
        }
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidDisputeId));
    };

    // Get order from db using the dispute order id
    let order = if let Some(order) = Order::by_id(pool, dispute.order_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
    {
        order
    } else {
        return Err(MostroCantDo(CantDoReason::NotFound));
    };

    // Update dispute fields
    dispute.status = Status::InProgress.to_string();
    dispute.solver_pubkey = Some(event.identity.to_string());
    dispute.taken_at = Timestamp::now().as_secs() as i64;

    info!("Dispute {} taken by {}", dispute.id, event.identity);

    // Save it to DB
    dispute
        .clone()
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Prepare payload for solver information message
    let dispute_info = prepare_solver_info_message(pool, &order, &dispute).await?;

    // We create a Message for admin
    let message = Message::new_dispute(
        Some(dispute.id),
        request_id,
        None,
        Action::AdminTookDispute,
        Some(Payload::Dispute(dispute.id, Some(dispute_info))),
    );
    let message = message
        .as_json()
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
    // Send the message to admin
    send_dm(event.identity, mostro_keys, &message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Now we create a message to both parties of the order
    // to them know who will assist them on the dispute
    let msg_to_users = Message::new_order(
        Some(order.id),
        request_id,
        None,
        Action::AdminTookDispute,
        Some(Payload::Peer(Peer {
            pubkey: event.identity.to_hex(),
            reputation: None,
        })),
    )
    .as_json()
    .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;

    // Send to buyer
    send_dm(
        order.get_buyer_pubkey().map_err(MostroInternalErr)?,
        mostro_keys,
        &msg_to_users,
        None,
    )
    .await
    .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Send message to seller
    send_dm(
        order.get_seller_pubkey().map_err(MostroInternalErr)?,
        mostro_keys,
        &msg_to_users,
        None,
    )
    .await
    .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Get the creator of the dispute
    let dispute_initiator = match (order.seller_dispute, order.buyer_dispute) {
        (true, false) => "seller",
        (false, true) => "buyer",
        (_, _) => return Err(MostroInternalErr(ServiceError::DisputeEventError)),
    };

    // We create a tag to show status of the dispute
    let tags: Tags = Tags::from_list(vec![
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("s")),
            vec![Status::InProgress.to_string()],
        ),
        // Who is the dispute creator
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("initiator")),
            vec![dispute_initiator],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("y")),
            create_platform_tag_values(ctx.settings().mostro.name.as_deref()),
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("z")),
            vec!["dispute".to_string()],
        ),
    ]);
    // nip33 kind with dispute id as identifier (kind 38386 for disputes)
    let event = new_dispute_event(mostro_keys, "", dispute.id.to_string(), tags)
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    info!("Dispute event to be published: {event:#?}");

    let client = ctx.nostr_client();
    client
        .send_event(&event)
        .await
        .map_err(|e| {
            info!("Failed to send dispute {} status event: {}", dispute.id, e);
            e
        })
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::admin_add_solver::{SOLVER_CATEGORY_READ_ONLY, SOLVER_CATEGORY_READ_WRITE};
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::db::add_new_user;
    use mostro_core::user::User;
    use sqlx::SqlitePool;
    use std::sync::Arc;

    async fn create_test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    async fn insert_solver(pool: &SqlitePool, pubkey: &str, category: i64) {
        add_new_user(pool, User::new(pubkey.to_string(), 0, 1, 0, category, 0))
            .await
            .unwrap();
    }

    fn build_ctx(pool: &SqlitePool) -> AppContext {
        // The DM/publish path reads the global config (event expiration);
        // seed it once, ignoring the error when another test already did.
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        TestContextBuilder::new()
            .with_pool(Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build()
    }

    /// Build an `UnwrappedMessage` whose seal signer (`identity`) is
    /// `identity` — `admin_take_dispute_action` gates on the identity key.
    fn create_admin_event(identity: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::new_dispute(None, Some(1), None, Action::AdminTakeDispute, None),
            signature: None,
            sender: identity,
            identity,
            created_at: Timestamp::now(),
        }
    }

    fn create_dispute_order(buyer: PublicKey, seller: PublicKey) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::Dispute.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: seller.to_string(),
            seller_pubkey: Some(seller.to_string()),
            buyer_pubkey: Some(buyer.to_string()),
            amount: 21_000,
            ..Default::default()
        }
    }

    fn take_dispute_msg(dispute_id: Option<uuid::Uuid>) -> Message {
        Message::new_dispute(dispute_id, Some(1), None, Action::AdminTakeDispute, None)
    }

    // ------------------------------------------------------------------
    // prepare_solver_info_message
    // ------------------------------------------------------------------

    /// Seller-initiated dispute where both parties are in full privacy mode
    /// (no master pubkeys): no user rows are looked up and both privacy
    /// flags stay set.
    #[tokio::test]
    async fn prepare_solver_info_message_seller_dispute_full_privacy() {
        let pool = create_test_pool().await;
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        let mut order = create_dispute_order(buyer, seller);
        order.seller_dispute = true;
        let dispute = Dispute::new(order.id, Status::Active.to_string());

        let info = prepare_solver_info_message(&pool, &order, &dispute)
            .await
            .unwrap();

        assert_eq!(info.initiator_pubkey, seller.to_string());
        assert!(info.initiator_full_privacy);
        assert!(info.counterpart_full_privacy);
        assert!(info.initiator_info.is_none());
        assert!(info.counterpart_info.is_none());
    }

    /// Buyer-initiated dispute where the buyer is an identified user
    /// (master pubkey differs from trade pubkey and has a user row): the
    /// initiator's reputation snapshot is included, the seller stays in
    /// full privacy.
    #[tokio::test]
    async fn prepare_solver_info_message_buyer_dispute_with_identified_buyer() {
        let pool = create_test_pool().await;
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();
        let buyer_master = Keys::generate().public_key();

        add_new_user(&pool, User::new(buyer_master.to_string(), 0, 0, 0, 0, 0))
            .await
            .unwrap();

        let mut order = create_dispute_order(buyer, seller);
        order.buyer_dispute = true;
        order.master_buyer_pubkey = Some(buyer_master.to_string());
        let dispute = Dispute::new(order.id, Status::Active.to_string());

        let info = prepare_solver_info_message(&pool, &order, &dispute)
            .await
            .unwrap();

        assert_eq!(info.initiator_pubkey, buyer.to_string());
        assert!(!info.initiator_full_privacy);
        assert!(info.initiator_info.is_some());
        assert!(info.counterpart_full_privacy);
        assert!(info.counterpart_info.is_none());
    }

    #[tokio::test]
    async fn prepare_solver_info_message_without_dispute_flags_fails() {
        let pool = create_test_pool().await;
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        // Neither seller_dispute nor buyer_dispute set
        let order = create_dispute_order(buyer, seller);
        let dispute = Dispute::new(order.id, Status::Active.to_string());

        let result = prepare_solver_info_message(&pool, &order, &dispute).await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::DisputeEventError))
        ));
    }

    /// A master pubkey that has no matching user row makes `is_user_present`
    /// fail and the error must propagate.
    #[tokio::test]
    async fn prepare_solver_info_message_propagates_missing_user_row() {
        let pool = create_test_pool().await;
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        let mut order = create_dispute_order(buyer, seller);
        order.seller_dispute = true;
        order.master_seller_pubkey = Some(Keys::generate().public_key().to_string());
        let dispute = Dispute::new(order.id, Status::Active.to_string());

        let result = prepare_solver_info_message(&pool, &order, &dispute).await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::DbAccessError(_)))
        ));
    }

    // ------------------------------------------------------------------
    // pubkey_event_can_solve
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn mostro_admin_can_solve_initiated_and_inprogress_disputes() {
        let pool = create_test_pool().await;
        let mostro_keys = Keys::generate();

        for status in [DisputeStatus::Initiated, DisputeStatus::InProgress] {
            assert!(
                pubkey_event_can_solve(
                    &pool,
                    &mostro_keys.public_key(),
                    status,
                    None,
                    &mostro_keys
                )
                .await
            );
        }
    }

    /// The admin shortcut only applies to Initiated/InProgress; for any
    /// other status the admin key falls through to the solver lookup and
    /// fails because it has no user row.
    #[tokio::test]
    async fn mostro_admin_cannot_solve_settled_dispute() {
        let pool = create_test_pool().await;
        let mostro_keys = Keys::generate();

        assert!(
            !pubkey_event_can_solve(
                &pool,
                &mostro_keys.public_key(),
                DisputeStatus::Settled,
                None,
                &mostro_keys,
            )
            .await
        );
    }

    #[tokio::test]
    async fn unknown_pubkey_cannot_solve() {
        let pool = create_test_pool().await;

        assert!(
            !pubkey_event_can_solve(
                &pool,
                &Keys::generate().public_key(),
                DisputeStatus::Initiated,
                None,
                &Keys::generate(),
            )
            .await
        );
    }

    /// A user row with `is_solver = 0` never comes back from
    /// `find_solver_pubkey` (the query filters on `is_solver == true`), so
    /// the sender is rejected.
    #[tokio::test]
    async fn non_solver_user_cannot_solve() {
        let pool = create_test_pool().await;
        let user_keys = Keys::generate();

        add_new_user(
            &pool,
            User::new(user_keys.public_key().to_string(), 0, 0, 0, 0, 0),
        )
        .await
        .unwrap();

        assert!(
            !pubkey_event_can_solve(
                &pool,
                &user_keys.public_key(),
                DisputeStatus::Initiated,
                None,
                &Keys::generate(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn any_solver_can_take_initiated_dispute() {
        let pool = create_test_pool().await;
        let solver_keys = Keys::generate();
        insert_solver(
            &pool,
            &solver_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_ONLY,
        )
        .await;

        assert!(
            pubkey_event_can_solve(
                &pool,
                &solver_keys.public_key(),
                DisputeStatus::Initiated,
                None,
                &Keys::generate(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn solver_cannot_act_on_settled_dispute() {
        let pool = create_test_pool().await;
        let solver_keys = Keys::generate();
        insert_solver(
            &pool,
            &solver_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_WRITE,
        )
        .await;

        assert!(
            !pubkey_event_can_solve(
                &pool,
                &solver_keys.public_key(),
                DisputeStatus::Settled,
                None,
                &Keys::generate(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn solver_cannot_act_on_inprogress_dispute_without_assigned_solver() {
        let pool = create_test_pool().await;
        let solver_keys = Keys::generate();
        insert_solver(
            &pool,
            &solver_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_WRITE,
        )
        .await;

        assert!(
            !pubkey_event_can_solve(
                &pool,
                &solver_keys.public_key(),
                DisputeStatus::InProgress,
                None,
                &Keys::generate(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn currently_assigned_solver_can_continue_inprogress_dispute() {
        let pool = create_test_pool().await;
        let solver_keys = Keys::generate();
        insert_solver(
            &pool,
            &solver_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_ONLY,
        )
        .await;

        let current = solver_keys.public_key().to_string();
        assert!(
            pubkey_event_can_solve(
                &pool,
                &solver_keys.public_key(),
                DisputeStatus::InProgress,
                Some(current.as_str()),
                &Keys::generate(),
            )
            .await
        );
    }

    /// Takeover requires the current solver to exist in the users table;
    /// an unknown current solver blocks the takeover.
    #[tokio::test]
    async fn takeover_fails_when_current_solver_is_unknown() {
        let pool = create_test_pool().await;
        let write_keys = Keys::generate();
        insert_solver(
            &pool,
            &write_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_WRITE,
        )
        .await;

        let ghost_solver = Keys::generate().public_key().to_string();
        assert!(
            !pubkey_event_can_solve(
                &pool,
                &write_keys.public_key(),
                DisputeStatus::InProgress,
                Some(ghost_solver.as_str()),
                &Keys::generate(),
            )
            .await
        );
    }

    // ------------------------------------------------------------------
    // admin_take_dispute_action
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn admin_take_dispute_action_without_dispute_id_fails() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let mostro_keys = Keys::generate();
        let event = create_admin_event(mostro_keys.public_key());

        let result =
            admin_take_dispute_action(&ctx, take_dispute_msg(None), &event, &mostro_keys).await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidDisputeId))
        ));
    }

    #[tokio::test]
    async fn admin_take_dispute_action_with_unknown_dispute_id_fails() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let mostro_keys = Keys::generate();
        let event = create_admin_event(mostro_keys.public_key());

        let result = admin_take_dispute_action(
            &ctx,
            take_dispute_msg(Some(uuid::Uuid::new_v4())),
            &event,
            &mostro_keys,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidDisputeId))
        ));
    }

    /// A dispute row whose status string cannot be parsed as a
    /// `DisputeStatus` must be rejected with `InvalidDisputeId`.
    #[tokio::test]
    async fn admin_take_dispute_action_with_garbage_dispute_status_fails() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let mostro_keys = Keys::generate();
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        let order = create_dispute_order(buyer, seller)
            .create(&pool)
            .await
            .unwrap();
        let mut dispute = Dispute::new(order.id, Status::Active.to_string())
            .create(&pool)
            .await
            .unwrap();
        dispute.status = "garbage-status".to_string();
        dispute.clone().update(&pool).await.unwrap();

        let event = create_admin_event(mostro_keys.public_key());
        let result = admin_take_dispute_action(
            &ctx,
            take_dispute_msg(Some(dispute.id)),
            &event,
            &mostro_keys,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidDisputeId))
        ));
    }

    #[tokio::test]
    async fn admin_take_dispute_action_rejects_unauthorized_pubkey() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let mostro_keys = Keys::generate();
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        let order = create_dispute_order(buyer, seller)
            .create(&pool)
            .await
            .unwrap();
        let dispute = Dispute::new(order.id, Status::Active.to_string())
            .create(&pool)
            .await
            .unwrap();

        // Identity is neither the admin key nor a solver
        let event = create_admin_event(Keys::generate().public_key());
        let result = admin_take_dispute_action(
            &ctx,
            take_dispute_msg(Some(dispute.id)),
            &event,
            &mostro_keys,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
    }

    #[tokio::test]
    async fn admin_take_dispute_action_fails_when_order_is_missing() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let mostro_keys = Keys::generate();
        let solver_keys = Keys::generate();
        insert_solver(
            &pool,
            &solver_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_WRITE,
        )
        .await;

        // Dispute referencing an order id that has no order row
        let dispute = Dispute::new(uuid::Uuid::new_v4(), Status::Active.to_string())
            .create(&pool)
            .await
            .unwrap();

        let event = create_admin_event(solver_keys.public_key());
        let result = admin_take_dispute_action(
            &ctx,
            take_dispute_msg(Some(dispute.id)),
            &event,
            &mostro_keys,
        )
        .await;

        assert!(matches!(result, Err(MostroCantDo(CantDoReason::NotFound))));
    }

    /// Happy path up to the final Nostr publish: the dispute is assigned to
    /// the solver and persisted, the solver info message is built and the
    /// three DMs succeed offline; the last `send_event` fails because the
    /// default client has no relays, so the function ends in `NostrError`.
    /// The trailing `Ok(())` is unreachable in unit tests.
    #[tokio::test]
    async fn admin_take_dispute_action_assigns_solver_and_persists_dispute() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let mostro_keys = Keys::generate();
        let solver_keys = Keys::generate();
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        insert_solver(
            &pool,
            &solver_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_WRITE,
        )
        .await;

        let mut order = create_dispute_order(buyer, seller);
        order.buyer_dispute = true;
        let order = order.create(&pool).await.unwrap();
        let dispute = Dispute::new(order.id, Status::Active.to_string())
            .create(&pool)
            .await
            .unwrap();

        let event = create_admin_event(solver_keys.public_key());
        let result = admin_take_dispute_action(
            &ctx,
            take_dispute_msg(Some(dispute.id)),
            &event,
            &mostro_keys,
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::NostrError(_)))
        ));

        // All DB side effects happened before the failing publish
        let stored = Dispute::by_id(&pool, dispute.id).await.unwrap().unwrap();
        assert_eq!(stored.status, DisputeStatus::InProgress.to_string());
        assert_eq!(
            stored.solver_pubkey,
            Some(solver_keys.public_key().to_string())
        );
        assert!(stored.taken_at > 0);
    }

    #[tokio::test]
    async fn write_solver_can_take_over_inprogress_from_read_only_solver() {
        let pool = create_test_pool().await;
        let mostro_keys = Keys::generate();
        let read_only_keys = Keys::generate();
        let write_keys = Keys::generate();

        insert_solver(
            &pool,
            &read_only_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_ONLY,
        )
        .await;
        insert_solver(
            &pool,
            &write_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_WRITE,
        )
        .await;

        let current_solver_pubkey = read_only_keys.public_key().to_string();
        let can_solve = pubkey_event_can_solve(
            &pool,
            &write_keys.public_key(),
            DisputeStatus::InProgress,
            Some(current_solver_pubkey.as_str()),
            &mostro_keys,
        )
        .await;

        assert!(
            can_solve,
            "a write-capable solver must be able to take over an InProgress dispute currently assigned to a read-only solver"
        );
    }

    #[tokio::test]
    async fn read_only_solver_cannot_take_over_inprogress_from_read_only_solver() {
        let pool = create_test_pool().await;
        let mostro_keys = Keys::generate();
        let current_keys = Keys::generate();
        let other_keys = Keys::generate();

        insert_solver(
            &pool,
            &current_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_ONLY,
        )
        .await;
        insert_solver(
            &pool,
            &other_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_ONLY,
        )
        .await;

        let current_solver_pubkey = current_keys.public_key().to_string();
        let can_solve = pubkey_event_can_solve(
            &pool,
            &other_keys.public_key(),
            DisputeStatus::InProgress,
            Some(current_solver_pubkey.as_str()),
            &mostro_keys,
        )
        .await;

        assert!(
            !can_solve,
            "a read-only solver must not be able to take over an InProgress dispute from another read-only solver"
        );
    }

    #[tokio::test]
    async fn write_solver_cannot_take_over_inprogress_from_write_solver() {
        let pool = create_test_pool().await;
        let mostro_keys = Keys::generate();
        let current_keys = Keys::generate();
        let other_keys = Keys::generate();

        insert_solver(
            &pool,
            &current_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_WRITE,
        )
        .await;
        insert_solver(
            &pool,
            &other_keys.public_key().to_string(),
            SOLVER_CATEGORY_READ_WRITE,
        )
        .await;

        let current_solver_pubkey = current_keys.public_key().to_string();
        let can_solve = pubkey_event_can_solve(
            &pool,
            &other_keys.public_key(),
            DisputeStatus::InProgress,
            Some(current_solver_pubkey.as_str()),
            &mostro_keys,
        )
        .await;

        assert!(
            !can_solve,
            "a write-capable solver must not be able to take over an InProgress dispute already held by another write-capable solver"
        );
    }
}
