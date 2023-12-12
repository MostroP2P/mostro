use crate::util::send_dm;

use anyhow::Result;
use mostro_core::dispute::{Dispute, Status};
use mostro_core::message::{Action, Content, Message};
use mostro_core::order::Order;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

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
            let message = Message::cant_do(None, None, None);
            let message = message.as_json()?;
            send_dm(client, my_keys, &event.pubkey, message).await?;

            return Ok(());
        }
    };
    let order = Order::by_id(pool, dispute.order_id).await?.unwrap();
    let order = order.as_new_order();

    let mostro_pubkey = my_keys.public_key().to_bech32()?;
    // Check if the pubkey is Mostro
    // TODO: solvers also can take disputes
    if event.pubkey.to_bech32()? != mostro_pubkey {
        // We create a Message
        let message = Message::cant_do(None, None, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }

    // Update dispute fields
    dispute.status = Status::InProgress;
    dispute.solver_pubkey = Some(event.pubkey.to_bech32()?);
    dispute.taken_at = Timestamp::now().as_i64();
    // Save it to DB
    dispute.update(pool).await?;

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

    Ok(())
}
