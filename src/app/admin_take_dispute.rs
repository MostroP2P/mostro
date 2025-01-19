use crate::db::find_solver_pubkey;
use crate::nip33::new_event;
use crate::util::{get_nostr_client, get_order, send_dm};

use mostro_core::dispute::{Dispute, Status};
use mostro_core::error::{MostroError::{self,*}, CantDoReason, ServiceError};
use mostro_core::message::{Action, Message, Payload, Peer};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::info;

pub async fn pubkey_event_can_solve(
    pool: &Pool<Sqlite>,
    ev_pubkey: &PublicKey,
    status: Status,
) -> bool {
    if let Ok(my_keys) = crate::util::get_keys() {
        // Is mostro admin taking dispute?
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
    let mut dispute = match Dispute::by_id(pool, dispute_id).await? {
        Some(dispute) => dispute,
        None => {
            return Err(MostroInternalErr(ServiceError::InvalidDisputeId));
        }
    };

    // Check if the pubkey is a solver or admin
    if let Ok(dispute_status) = Status::from_str(&dispute.status) {
        if !pubkey_event_can_solve(pool, &event.rumor.pubkey, dispute_status).await {
            // We create a Message
            return Err(MostroCantDo(CantDoReason::InvalidPubkey));
        }
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidDisputeId));
    };

    // Get order from db
    let order = get_order(&msg, pool).await?;

    let mut new_order = order.as_new_order();
    // Only in this case we use the trade pubkey fields to store the master pubkey
    new_order
        .buyer_trade_pubkey
        .clone_from(&order.master_buyer_pubkey);
    new_order
        .seller_trade_pubkey
        .clone_from(&order.master_seller_pubkey);

    // Update dispute fields
    dispute.status = Status::InProgress.to_string();
    dispute.solver_pubkey = Some(event.rumor.pubkey.to_string());
    dispute.taken_at = Timestamp::now().as_u64() as i64;

    info!("Dispute {} taken by {}", dispute_id, event.rumor.pubkey);
    // Assign token for admin message
    new_order.seller_token = dispute.seller_token;
    new_order.buyer_token = dispute.buyer_token;
    // Save it to DB
    dispute.update(pool).await?;

    // We create a Message for admin
    let message = Message::new_dispute(
        Some(dispute_id),
        request_id,
        None,
        Action::AdminTookDispute,
        Some(Payload::Order(new_order)),
    );
    let message = message.as_json()?;
    let sender_keys = crate::util::get_keys().unwrap();
    send_dm(&event.rumor.pubkey, sender_keys, message, None).await?;
    // Now we create a message to both parties of the order
    // to them know who will assist them on the dispute
    let solver_pubkey = Peer::new(event.rumor.pubkey.to_hex());
    let msg_to_buyer = Message::new_order(
        Some(order.id),
        request_id,
        None,
        Action::AdminTookDispute,
        Some(Payload::Peer(solver_pubkey.clone())),
    );

    let msg_to_seller = Message::new_order(
        Some(order.id),
        request_id,
        None,
        Action::AdminTookDispute,
        Some(Payload::Peer(solver_pubkey)),
    );

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())?,
            PublicKey::from_str(buyer.as_str())?,
        ),
        (None, _) => return Err(Error::msg("Missing seller pubkey")),
        (_, None) => return Err(Error::msg("Missing buyer pubkey")),
    };
    let sender_keys = crate::util::get_keys().unwrap();
    send_dm(
        &buyer_pubkey,
        sender_keys.clone(),
        msg_to_buyer.as_json()?,
        None,
    )
    .await?;
    send_dm(&seller_pubkey, sender_keys, msg_to_seller.as_json()?, None).await?;
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
    let event = new_event(&crate::util::get_keys()?, "", dispute_id.to_string(), tags)?;
    info!("Dispute event to be published: {event:#?}");

    let client = get_nostr_client().map_err(|e| {
        info!(
            "Failed to get nostr client for dispute {}: {}",
            dispute_id, e
        );
        e
    })?;

    client.send_event(event).await.map_err(|e| {
        info!("Failed to send dispute {} status event: {}", dispute_id, e);
        e
    })?;

    Ok(())
}
