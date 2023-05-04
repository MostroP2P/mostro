use crate::lightning::LndConnector;
use crate::util::{send_dm, update_order_event};

use anyhow::Result;
use log::{error, info};
use mostro_core::order::Order;
use mostro_core::{Action, Message, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

pub async fn admin_cancel_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    let order_id = msg.order_id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("AdminCancel: Order Id {order_id} not found!");
            return Ok(());
        }
    };
    let mostro_pubkey = my_keys.public_key().to_bech32()?;
    // Check if the pubkey is Mostro
    if event.pubkey.to_bech32()? != mostro_pubkey {
        // We create a Message
        let message = Message::new(0, Some(order.id), None, Action::CantDo, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }

    if order.hash.is_some() {
        // We return funds to seller
        let hash = order.hash.as_ref().unwrap();
        ln_client.cancel_hold_invoice(hash).await?;
        info!(
            "AdminCancel: Order Id {}: Funds returned to seller",
            &order.id
        );
    }
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    update_order_event(pool, client, my_keys, Status::CanceledByAdmin, &order, None).await?;
    // We create a Message
    let message = Message::new(0, Some(order.id), None, Action::AdminCancel, None);
    let message = message.as_json()?;
    // Message to admin
    send_dm(client, my_keys, &event.pubkey, message.clone()).await?;
    let seller_pubkey = order.seller_pubkey.as_ref().unwrap();
    let seller_pubkey = XOnlyPublicKey::from_bech32(seller_pubkey).unwrap();
    send_dm(client, my_keys, &seller_pubkey, message.clone()).await?;
    let buyer_pubkey = order.buyer_pubkey.as_ref().unwrap();
    let buyer_pubkey = XOnlyPublicKey::from_bech32(buyer_pubkey).unwrap();
    send_dm(client, my_keys, &buyer_pubkey, message.clone()).await?;

    Ok(())
}
