use crate::db::find_solver_pubkey;
use crate::nip33::new_event;
use crate::util::{send_cant_do_msg, send_dm};
use crate::NOSTR_CLIENT;

use anyhow::{Error, Result};
use mostro_core::dispute::{Dispute, Status};
use mostro_core::message::{Action, Content, Message, Peer};
use mostro_core::order::Order;
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
    event: &Event,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    // Find dipute id in the message
    let dispute_id = if let Some(dispute_id) = msg.get_inner_message_kind().id {
        dispute_id
    } else {
        return Err(Error::msg("No order id"));
    };

    // Fetch dispute from db
    let mut dispute = match Dispute::by_id(pool, dispute_id).await? {
        Some(dispute) => dispute,
        None => {
            // We create a Message
            send_cant_do_msg(
                Some(dispute_id),
                Some("Dispute not found".to_string()),
                &event.pubkey,
            )
            .await;
            return Ok(());
        }
    };

    // Check if the pubkey is a solver or admin
    if let Ok(dispute_status) = Status::from_str(&dispute.status) {
        if !pubkey_event_can_solve(pool, &event.pubkey, dispute_status).await {
            // We create a Message
            send_cant_do_msg(None, Some("Not allowed".to_string()), &event.pubkey).await;
            return Ok(());
        }
    } else {
        return Err(Error::msg("No dispute status"));
    };

    let order = match Order::by_id(pool, dispute.order_id).await? {
        Some(o) => o,
        None => return Err(Error::msg("No order id")),
    };

    let mut new_order = order.as_new_order();
    new_order.master_buyer_pubkey = order.master_buyer_pubkey.clone();
    new_order.master_seller_pubkey = order.master_seller_pubkey.clone();

    // Update dispute fields
    dispute.status = Status::InProgress.to_string();
    dispute.solver_pubkey = Some(event.pubkey.to_string());
    dispute.taken_at = Timestamp::now().as_i64();
    // Save it to DB
    dispute.update(pool).await?;
    info!("Dispute {} taken by {}", dispute_id, event.pubkey);
    // We create a Message for admin
    let message = Message::new_dispute(
        Some(dispute_id),
        None,
        Action::AdminTookDispute,
        Some(Content::Order(new_order)),
    );
    let message = message.as_json()?;
    send_dm(&event.pubkey, message).await?;
    // Now we create a message to both parties of the order
    // to them know who will assist them on the dispute
    let solver_pubkey = Peer::new(event.pubkey.to_hex());
    let message = Message::new_order(
        Some(order.id),
        None,
        Action::AdminTookDispute,
        Some(Content::Peer(solver_pubkey)),
    );

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())?,
            PublicKey::from_str(buyer.as_str())?,
        ),
        (None, _) => return Err(Error::msg("Missing seller pubkey")),
        (_, None) => return Err(Error::msg("Missing buyer pubkey")),
    };

    let message = message.as_json()?;
    send_dm(&buyer_pubkey, message.clone()).await?;
    send_dm(&seller_pubkey, message).await?;
    // We create a tag to show status of the dispute
    let tags = vec![
        ("s".to_string(), Status::InProgress.to_string()),
        ("y".to_string(), "mostrop2p".to_string()),
        ("z".to_string(), "dispute".to_string()),
    ];
    // nip33 kind with dispute id as identifier
    let event = new_event(&crate::util::get_keys()?, "", dispute_id.to_string(), tags)?;
    info!("Dispute event to be published: {event:#?}");
    NOSTR_CLIENT.get().unwrap().send_event(event).await?;

    Ok(())
}
