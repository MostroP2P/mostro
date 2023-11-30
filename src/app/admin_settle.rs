use crate::db::find_dispute_by_order_id;
use crate::lightning::LndConnector;
use crate::util::{send_dm, settle_seller_hold_invoice};

use anyhow::Result;
use log::error;
use mostro_core::dispute::Status as DisputeStatus;
use mostro_core::message::{Action, Message};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

pub async fn admin_settle_action(
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
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };
    let status = Status::SettledByAdmin;
    let action = Action::AdminSettle;

    settle_seller_hold_invoice(
        event, my_keys, client, pool, ln_client, status, action, true, &order,
    )
    .await?;
    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order_id).await;

    if let Ok(mut d) = dispute {
        // we update the dispute
        d.status = DisputeStatus::Settled;
        d.update(pool).await?;
    }
    // We create a Message
    let message = Message::new(Some(order.id), None, Action::AdminSettle, None);
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
