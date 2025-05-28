use crate::db::{find_solver_pubkey, is_user_present};
use crate::nip33::new_event;
use crate::util::{get_dispute, get_nostr_client, send_dm};
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::info;

async fn prepare_solver_info_message(
    pool: &Pool<Sqlite>,
    order: &Order,
    dispute: &Dispute,
) -> Result<SolverDisputeInfo, MostroError> {
    // Check if one or both users are in full privacy mode
    let (full_privacy_buyer, full_privacy_seller) = order.is_full_privacy_order();

    // Get master pubkeys to get users data from db
    let master_buyer_key = &order
        .get_master_buyer_pubkey()
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;

    let master_seller_key: &PublicKey = &order
        .get_master_seller_pubkey()
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;

    // Get pubkeys of initiator and counterpart and users data if not in full privacy mode
    let (initiator_tradekey, initiator, counterpart) = if order.buyer_dispute {
        (
            order
                .get_buyer_pubkey()
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?
                .to_string(),
            if !full_privacy_buyer {
                Some(
                    is_user_present(pool, master_buyer_key.to_string())
                        .await
                        .map_err(|cause| {
                            MostroInternalErr(ServiceError::DbAccessError(cause.to_string()))
                        })?,
                )
            } else {
                None
            },
            if !full_privacy_seller {
                Some(
                    is_user_present(pool, master_seller_key.to_string())
                        .await
                        .map_err(|cause| {
                            MostroInternalErr(ServiceError::DbAccessError(cause.to_string()))
                        })?,
                )
            } else {
                None
            },
        )
    } else {
        (
            order
                .get_seller_pubkey()
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?
                .to_string(),
            if !full_privacy_seller {
                Some(
                    is_user_present(pool, master_seller_key.to_string())
                        .await
                        .map_err(|cause| {
                            MostroInternalErr(ServiceError::DbAccessError(cause.to_string()))
                        })?,
                )
            } else {
                None
            },
            if !full_privacy_buyer {
                Some(
                    is_user_present(pool, master_buyer_key.to_string())
                        .await
                        .map_err(|cause| {
                            MostroInternalErr(ServiceError::DbAccessError(cause.to_string()))
                        })?,
                )
            } else {
                None
            },
        )
    };

    // Prepare dispute info
    let dispute_info = SolverDisputeInfo::new(
        order,
        dispute,
        initiator_tradekey.clone(),
        counterpart,
        initiator,
    );

    Ok(dispute_info)
}

pub async fn pubkey_event_can_solve(
    pool: &Pool<Sqlite>,
    ev_pubkey: &PublicKey,
    status: DisputeStatus,
) -> bool {
    if let Ok(my_keys) = crate::util::get_keys() {
        // Is mostro admin taking dispute?
        info!(
            "admin pubkey {} -event pubkey {} ",
            my_keys.public_key.to_string(),
            ev_pubkey.to_string()
        );
        if ev_pubkey.to_string() == my_keys.public_key().to_string()
            && matches!(status, DisputeStatus::InProgress | DisputeStatus::Initiated)
        {
            return true;
        }
    }

    // Is a solver taking a dispute
    if let Ok(solver) = find_solver_pubkey(pool, ev_pubkey.to_string()).await {
        if solver.is_solver != 0_i64 && status == DisputeStatus::Initiated {
            return true;
        }
    }

    false
}

pub async fn admin_take_dispute_action(
    msg: Message,
    event: &UnwrappedGift,
    mostro_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    // Get dispute
    let mut dispute = get_dispute(&msg, pool).await?;

    // Check if the pubkey is a solver or admin
    if let Ok(dispute_status) = DisputeStatus::from_str(&dispute.status) {
        if !pubkey_event_can_solve(pool, &event.sender, dispute_status).await {
            // We create a Message
            return Err(MostroCantDo(CantDoReason::InvalidPubkey));
        }
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidDisputeId));
    };

    // Get order from db using the dispute order id
    let order = if let Some(order) = Order::by_id(pool, dispute.order_id)
        .await
        .map_err(|_| MostroInternalErr(ServiceError::InvalidOrderId))?
    {
        order
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidOrderId));
    };

    // Update dispute fields
    dispute.status = Status::InProgress.to_string();
    dispute.solver_pubkey = Some(event.sender.to_string());
    dispute.taken_at = Timestamp::now().as_u64() as i64;

    info!("Dispute {} taken by {}", dispute.id, event.sender);

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
        Some(Payload::Dispute(dispute.id, None, Some(dispute_info))),
    );
    let message = message
        .as_json()
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
    // Send the message to admin
    send_dm(event.sender, mostro_keys, &message, None)
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
            pubkey: event.sender.to_hex(),
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

    // We create a tag to show status of the dispute
    let tags: Tags = Tags::from_list(vec![
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("s")),
            vec![Status::InProgress.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("y")),
            vec!["mostro".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("z")),
            vec!["dispute".to_string()],
        ),
    ]);
    // nip33 kind with dispute id as identifier
    let event = new_event(
        &crate::util::get_keys()
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?,
        "",
        dispute.id.to_string(),
        tags,
    )
    .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    info!("Dispute event to be published: {event:#?}");

    let client = get_nostr_client()
        .map_err(|e| {
            info!(
                "Failed to get nostr client for dispute {}: {}",
                dispute.id, e
            );
            e
        })
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

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
