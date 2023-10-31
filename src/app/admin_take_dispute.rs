use crate::db::take_dispute;
use crate::util::send_dm;

use anyhow::Result;
use log::error;
use mostro_core::user::User;
use mostro_core::{Action, Content, Message};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

pub async fn admin_take_dispute(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let content = if let Some(content) = msg.content {
        content
    } else {
        error!("AdminTakeDispute: No dispute id found!");
        return Ok(());
    };
    let dispute_id = if let Content::TextMessage(d) = content {
        d
    } else {
        error!("AdminTakeDispute: No dispute id found!");
        return Ok(());
    };

    let mostro_pubkey = my_keys.public_key().to_bech32()?;
    // Check if the pubkey is Mostro
    // TODO: solvers also can take disputes
    if event.pubkey.to_bech32()? != mostro_pubkey {
        // We create a Message
        let message = Message::new(0, None, None, Action::CantDo, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }
    take_dispute(pool).await?;

    // We create a Message for admin
    let message = Message::new(0, None, None, Action::AdminAddSolver, None);
    let message = message.as_json()?;
    // Send the message
    send_dm(client, my_keys, &event.pubkey, message.clone()).await?;

    Ok(())
}
