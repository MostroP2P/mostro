use crate::db::add_user;
use crate::util::send_dm;

use anyhow::Result;
use log::error;
use mostro_core::user::User;
use mostro_core::{Action, Content, Message};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

pub async fn admin_add_solver_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let content = if let Some(content) = msg.content {
        content
    } else {
        error!("No pubkey found!");
        return Ok(());
    };
    let npubkey = if let Content::TextMessage(p) = content {
        p
    } else {
        error!("No pubkey found!");
        return Ok(());
    };

    let mostro_pubkey = my_keys.public_key().to_bech32()?;
    // Check if the pubkey is Mostro
    if event.pubkey.to_bech32()? != mostro_pubkey {
        // We create a Message
        let message = Message::new(0, None, None, Action::CantDo, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }
    let user = User::new(npubkey, 0, 1, 0, 0);
    add_user(&user, pool).await?;

    // We create a Message for admin
    let message = Message::new(0, None, None, Action::AdminAddSolver, None);
    let message = message.as_json()?;
    // Send the message
    send_dm(client, my_keys, &event.pubkey, message.clone()).await?;

    Ok(())
}
