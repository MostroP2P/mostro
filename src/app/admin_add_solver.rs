use crate::db::add_new_user;
use crate::util::send_dm;
use mostro_core::prelude::*;
use mostro_core::user::User;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use tracing::{error, info};

pub async fn admin_add_solver_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    let inner_message = msg.get_inner_message_kind();
    let payload = if let Some(payload) = &inner_message.payload {
        payload
    } else {
        error!("No pubkey found!");
        return Ok(());
    };
    let npubkey = if let Payload::TextMessage(p) = payload {
        p
    } else {
        error!("No pubkey found!");
        return Ok(());
    };

    // Check if the pubkey is Mostro
    if event.sender.to_string() != my_keys.public_key().to_string() {
        // We create a Message
        return Err(MostroInternalErr(ServiceError::InvalidPubkey));
    }
    let trade_index = inner_message.trade_index.unwrap_or(0);
    let public_key = PublicKey::from_bech32(npubkey)
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;
    let user = User::new(public_key.to_string(), 0, 1, 0, 0, trade_index);
    // Use CRUD to create user
    match add_new_user(pool, user).await {
        Ok(r) => info!("Solver added: {:#?}", r),
        Err(ee) => error!("Error creating solver: {:#?}", ee),
    }
    // We create a Message for admin
    let message = Message::new_dispute(None, request_id, None, Action::AdminAddSolver, None);
    let message = message
        .as_json()
        .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
    // Send the message
    send_dm(event.rumor.pubkey, my_keys, &message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    Ok(())
}
