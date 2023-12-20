use crate::nip33::new_event;
use crate::util::send_dm;

use anyhow::Result;
use mostro_core::dispute::{Dispute, Status};
use mostro_core::message::{Action, Content, Message};
use mostro_core::order::Order;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tracing::info;

pub async fn admin_take_dispute_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let dispute_id = msg.get_inner_message_kind().id.unwrap();
    let mut dispute = match Dispute::by_id(pool, dispute_id).await? {
        Some(dispute) => dispute,
        None => {
            // We create a Message
            let message = Message::cant_do(
                None,
                None,
                Some(Content::TextMessage("Dispute not found".to_string())),
            );
            let message = message.as_json()?;
            send_dm(client, my_keys, &event.pubkey, message).await?;

            return Ok(());
        }
    };
    let order = Order::by_id(pool, dispute.order_id).await?.unwrap();
    let order = order.as_new_order();

    // Check if the pubkey is Mostro
    // TODO: solvers also can take disputes
    if event.pubkey.to_string() != my_keys.public_key().to_string() {
        // We create a Message
        let message = Message::cant_do(
            None,
            None,
            Some(Content::TextMessage("Not allowed".to_string())),
        );
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }

    // Update dispute fields
    dispute.status = Status::InProgress;
    dispute.solver_pubkey = Some(event.pubkey.to_string());
    dispute.taken_at = Timestamp::now().as_i64();
    // Save it to DB
    dispute.update(pool).await?;
    info!("Dispute {} taken by {}", dispute_id, event.pubkey);
    // We create a Message for admin
    let message = Message::new_dispute(
        Some(dispute_id),
        None,
        Action::AdminTakeDispute,
        Some(Content::Order(order)),
    );
    let message = message.as_json()?;
    // Send the message
    send_dm(client, my_keys, &event.pubkey, message.clone()).await?;

    // We create a tag to show status of the dispute
    let tags = vec![
        ("s".to_string(), "InProgress".to_string()),
        ("y".to_string(), "mostrop2p".to_string()),
        ("z".to_string(), "dispute".to_string()),
    ];
    // nip33 kind with dispute id as identifier
    let event = new_event(my_keys, "", dispute_id.to_string(), tags)?;
    info!("Dispute event to be published: {event:#?}");
    client.send_event(event).await?;

    Ok(())
}
