use crate::db::find_dispute_by_order_id;
use crate::lightning::LndConnector;
use crate::lnurl::resolv_ln_address;
use crate::nip33::new_event;
use crate::util::{
    connect_nostr, get_keys, rate_counterpart, send_dm, settle_seller_hold_invoice,
    update_order_event,
};

use anyhow::Result;
use lnurl::lightning_address::LightningAddress;
use mostro_core::dispute::Status as DisputeStatus;
use mostro_core::message::{Action, Message};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tokio::sync::mpsc::channel;
use tonic_openssl_lnd::lnrpc::payment::PaymentStatus;
use tracing::{error, info};

pub async fn admin_settle_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    let order_id = msg.get_inner_message_kind().id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };

    settle_seller_hold_invoice(
        event,
        my_keys,
        client,
        ln_client,
        Action::AdminSettle,
        true,
        &order,
    )
    .await?;
    let cloned_order = order.clone();
    // Update order event
    let order = update_order_event(client, my_keys, Status::SettledByAdmin, &order).await?;
    order.update(pool).await?;

    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order_id).await;

    if let Ok(mut d) = dispute {
        let dispute_id = d.id;
        // we update the dispute
        d.status = DisputeStatus::Settled;
        d.update(pool).await?;
        // We create a tag to show status of the dispute
        let tags = vec![
            ("s".to_string(), "Settled".to_string()),
            ("y".to_string(), "mostrop2p".to_string()),
            ("z".to_string(), "dispute".to_string()),
        ];
        // nip33 kind with dispute id as identifier
        let event = new_event(my_keys, "", dispute_id.to_string(), tags)?;

        client.send_event(event).await?;
    }
    // We create a Message
    let message = Message::new_dispute(Some(cloned_order.id), None, Action::AdminSettle, None);
    let message = message.as_json()?;
    // Message to admin
    send_dm(client, my_keys, &event.pubkey, message.clone()).await?;
    let seller_pubkey = cloned_order.seller_pubkey.as_ref().unwrap();
    let seller_pubkey = XOnlyPublicKey::from_str(seller_pubkey).unwrap();
    send_dm(client, my_keys, &seller_pubkey, message.clone()).await?;
    let buyer_pubkey = cloned_order.buyer_pubkey.as_ref().unwrap();
    let buyer_pubkey = XOnlyPublicKey::from_str(buyer_pubkey).unwrap();
    send_dm(client, my_keys, &buyer_pubkey, message.clone()).await?;

    // Finally we try to pay buyer's invoice
    let payment_request = cloned_order.buyer_invoice.as_ref().unwrap().to_string();
    let ln_addr = LightningAddress::from_str(&payment_request);
    let payment_request = if let Ok(addr) = ln_addr {
        let amount = cloned_order.amount as u64 - cloned_order.fee as u64;
        resolv_ln_address(&addr.to_string(), amount).await?
    } else {
        payment_request
    };

    let mut ln_client_payment = LndConnector::new().await;
    let (tx, mut rx) = channel(100);
    let payment_task = {
        async move {
            let _ = ln_client_payment
                .send_payment(&payment_request, cloned_order.amount, tx)
                .await;
        }
    };
    tokio::spawn(payment_task);
    let payment = {
        async move {
            // We redeclare vars to use inside this block
            let client = connect_nostr().await.unwrap();
            let my_keys = get_keys().unwrap();
            let buyer_pubkey =
                XOnlyPublicKey::from_str(cloned_order.buyer_pubkey.as_ref().unwrap()).unwrap();
            // Receiving msgs from send_payment()
            while let Some(msg) = rx.recv().await {
                if let Some(status) = PaymentStatus::from_i32(msg.payment.status) {
                    if status == PaymentStatus::Succeeded {
                        info!(
                            "Order Id {}: Invoice with hash: {} paid!",
                            cloned_order.id, msg.payment.payment_hash
                        );
                        // Purchase completed message to buyer
                        let message = Message::new_order(
                            Some(cloned_order.id),
                            None,
                            Action::PurchaseCompleted,
                            None,
                        );
                        let message = message.as_json().unwrap();
                        send_dm(&client, &my_keys, &buyer_pubkey, message)
                            .await
                            .unwrap();
                        let status = Status::Success;
                        // Let's wait 10 secs before publish this new event
                        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                        // We publish a new replaceable kind nostr event with the status updated
                        // and update on local database the status and new event id
                        update_order_event(&client, &my_keys, status, &cloned_order)
                            .await
                            .unwrap();

                        // Adding here rate process
                        rate_counterpart(
                            &client,
                            &buyer_pubkey,
                            &seller_pubkey,
                            &my_keys,
                            &cloned_order,
                        )
                        .await
                        .unwrap();
                    }
                }
            }
        }
    };
    tokio::spawn(payment);

    Ok(())
}
