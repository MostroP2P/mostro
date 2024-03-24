use crate::util::{send_cant_do_msg, send_dm};

use anyhow::Result;
use mostro_core::message::{Action, Content, Message};
use mostro_core::user::User;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tracing::{error, info};

pub async fn admin_add_solver_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
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

    // Check if the pubkey is Mostro
    if event.pubkey.to_string() != my_keys.public_key().to_string() {
        // We create a Message
        send_cant_do_msg(None, Some("Not allowed".to_string()), &event.pubkey).await;
        return Ok(());
    }
    let user = User::new(npubkey.to_string(), 0, 1, 0, 0);
    // Use CRUD to create user
    match user.create(pool).await {
        Ok(r) => info!("Solver added: {:#?}", r),
        Err(ee) => error!("Error creating solver: {:#?}", ee),
    }
    // We create a Message for admin
    let message = Message::new_dispute(None, None, Action::AdminAddSolver, None);
    let message = message.as_json()?;
    // Send the message
    send_dm(&event.pubkey, message).await?;

    Ok(())
}
