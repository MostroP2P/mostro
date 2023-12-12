use crate::util::send_dm;

use anyhow::Result;
use mostro_core::message::{Action, Content, Message};
use mostro_core::user::User;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tracing::error;

pub async fn admin_add_solver_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let inner_message = msg.get_inner_message_kind();
    let content = if let Some(content) = &inner_message.content {
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
        let message = Message::cant_do(None, None, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }
    let user = User::new(npubkey.to_string(), 0, 1, 0, 0);
    // Use CRUD to create user
    user.create(pool).await?;

    // We create a Message for admin
    let message = Message::new_dispute(None, None, Action::AdminAddSolver, None);
    let message = message.as_json()?;
    // Send the message
    send_dm(client, my_keys, &event.pubkey, message.clone()).await?;

    Ok(())
}
