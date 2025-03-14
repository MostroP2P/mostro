use crate::db::{find_solver_pubkey, is_user_present};
use crate::nip33::new_event;
use crate::util::{get_nostr_client, get_order, send_dm};

use mostro_core::dispute::{Dispute, SolverDisputeInfo, Status};
use mostro_core::error::{
    CantDoReason,
    MostroError::{self, *},
    ServiceError,
};
use mostro_core::message::{Action, Message, Payload, Peer};
use mostro_core::order::Order;
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
    // Get pubkeys of initiator and counterpart
    let (initiator_pubkey, counterpart_pubkey) = if order.buyer_dispute {
        (
            &order
                .get_master_buyer_pubkey()
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            &order
                .get_master_seller_pubkey()
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
        )
    } else {
        (
            &order
                .get_master_seller_pubkey()
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            &order
                .get_buyer_pubkey()
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
        )
    };

    // Get users ratings
    // Get counter to vote from db

    let counterpart = is_user_present(pool, counterpart_pubkey.to_string())
        .await
        .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;

    let initiator = is_user_present(pool, initiator_pubkey.to_string())
        .await
        .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;

    // Calculate operating days of users
    let now = Timestamp::now();
    let initiator_operating_days = (now.as_u64() - initiator.created_at as u64) / 86400;
    let couterpart_operating_days = (now.as_u64() - counterpart.created_at as u64) / 86400;

    let dispute_info = SolverDisputeInfo::new(
        order,
        dispute,
        &counterpart,
        &initiator,
        initiator_operating_days,
        couterpart_operating_days,
    );

    Ok(dispute_info)
}

pub async fn pubkey_event_can_solve(
    pool: &Pool<Sqlite>,
    ev_pubkey: &PublicKey,
    status: Status,
) -> bool {
    if let Ok(my_keys) = crate::util::get_keys() {
        // Is mostro admin taking dispute?
        info!(
            "admin pubkey {} -event pubkey {} ",
            my_keys.public_key.to_string(),
            ev_pubkey.to_string()
        );
        if ev_pubkey.to_string() == my_keys.public_key().to_string()
            && matches!(status, Status::InProgress | Status::Initiated)
        {
            return true;
        }
    }

    // Is a solver taking a dispute
    if let Ok(solver) = find_solver_pubkey(pool, ev_pubkey.to_string()).await {
        if solver.is_solver != 0_i64 && status == Status::Initiated {
            return true;
        }
    }

    false
}

pub async fn admin_take_dispute_action(
    msg: Message,
    event: &UnwrappedGift,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    // Find dipute id in the message
    let dispute_id = if let Some(dispute_id) = msg.get_inner_message_kind().id {
        dispute_id
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidDisputeId));
    };

    // Fetch dispute from db
    let mut dispute = match Dispute::by_id(pool, dispute_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
    {
        Some(dispute) => dispute,
        None => {
            return Err(MostroInternalErr(ServiceError::InvalidDisputeId));
        }
    };

    // Check if the pubkey is a solver or admin
    if let Ok(dispute_status) = Status::from_str(&dispute.status) {
        if !pubkey_event_can_solve(pool, &event.sender, dispute_status).await {
            // We create a Message
            return Err(MostroCantDo(CantDoReason::InvalidPubkey));
        }
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidDisputeId));
    };

    // Get order from db using the dispute order id
    let order = get_order(&msg, pool).await?;

    // Update dispute fields
    dispute.status = Status::InProgress.to_string();
    dispute.solver_pubkey = Some(event.sender.to_string());
    dispute.taken_at = Timestamp::now().as_u64() as i64;

    info!("Dispute {} taken by {}", dispute_id, event.sender);

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
        Some(dispute_id),
        request_id,
        None,
        Action::AdminTookDispute,
        Some(Payload::Dispute(dispute_id, None, Some(dispute_info))),
    );
    let message = message
        .as_json()
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
    let sender_keys = crate::util::get_keys()?;
    send_dm(event.sender, sender_keys, message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    // Now we create a message to both parties of the order
    // to them know who will assist them on the dispute
    let admin_pubkey = Peer::new(event.sender.to_hex());
    let msg_to_buyer = Message::new_order(
        Some(order.id),
        request_id,
        None,
        Action::AdminTookDispute,
        Some(Payload::Peer(admin_pubkey.clone())),
    );

    let msg_to_seller = Message::new_order(
        Some(order.id),
        request_id,
        None,
        Action::AdminTookDispute,
        Some(Payload::Peer(admin_pubkey)),
    );

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            PublicKey::from_str(buyer.as_str())
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
        ),
        (None, _) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
        (_, None) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };
    let sender_keys = crate::util::get_keys()?;
    send_dm(
        buyer_pubkey,
        sender_keys.clone(),
        msg_to_buyer
            .as_json()
            .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?,
        None,
    )
    .await
    .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Send message to seller
    send_dm(
        seller_pubkey,
        sender_keys,
        msg_to_seller
            .as_json()
            .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?,
        None,
    )
    .await
    .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    // We create a tag to show status of the dispute
    let tags: Tags = Tags::new(vec![
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("s")),
            vec![Status::InProgress.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("y")),
            vec!["mostrop2p".to_string()],
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
        dispute_id.to_string(),
        tags,
    )
    .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    info!("Dispute event to be published: {event:#?}");

    let client = get_nostr_client()
        .map_err(|e| {
            info!(
                "Failed to get nostr client for dispute {}: {}",
                dispute_id, e
            );
            e
        })
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    client
        .send_event(event)
        .await
        .map_err(|e| {
            info!("Failed to send dispute {} status event: {}", dispute_id, e);
            e
        })
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    Ok(())
}
