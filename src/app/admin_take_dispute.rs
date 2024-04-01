use crate::db::find_solver_npub;
use crate::nip33::new_event;
use crate::util::{send_cant_do_msg, send_dm};
use crate::NOSTR_CLIENT;

use anyhow::Result;
use mostro_core::dispute::{Dispute, Status};
use mostro_core::message::{Action, Content, Message};
use mostro_core::order::Order;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tracing::info;

pub async fn npub_event_can_solve(pool: &Pool<Sqlite>, ev_pubkey: &PublicKey) -> bool {
    if let Ok(my_keys) = crate::util::get_keys() {
        // Is mostro admin taking dispute?
        if ev_pubkey.to_string() == my_keys.public_key().to_string() {
            return true;
        }
    }

    // Is a solver taking a dispute
    if let Ok(solver) = find_solver_npub(pool, ev_pubkey.to_string()).await {
        if solver.is_solver != 0_i64 {
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
    let dispute_id = msg.get_inner_message_kind().id.unwrap();
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
    let order = Order::by_id(pool, dispute.order_id).await?.unwrap();
    let mut new_order = order.as_new_order();
    new_order.master_buyer_pubkey = order.master_buyer_pubkey.clone();
    new_order.master_seller_pubkey = order.master_seller_pubkey.clone();

    // Check if the pubkey is Mostro
    // TODO: solvers also can take disputes
    if !npub_event_can_solve(pool, &event.pubkey).await {
        // We create a Message
        send_cant_do_msg(None, Some("Not allowed".to_string()), &event.pubkey).await;
        return Ok(());
    }

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
    // Send the message
    send_dm(&event.pubkey, message.clone()).await?;

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
