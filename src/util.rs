use crate::models::Yadio;
use crate::{db, flow};
use anyhow::{Context, Result, Ok};
use dotenvy::var;
use log::{error, info};
use mostro_core::order::{NewOrder, Order, SmallOrder};
use mostro_core::{Action, Content, Kind as OrderKind, Message, Status};
use nostr_sdk::prelude::hex::ToHex;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
use std::str::FromStr;
use tonic_openssl_lnd::lnrpc::invoice::InvoiceState;

use crate::lightning;
use crate::messages;
use tokio::sync::mpsc::channel;
use uuid::Uuid;


/// Request market quote from Yadio to have sats amount at actual market price
pub async fn get_market_quote(fiat_amount: &i64, fiat_code: &str, premium: &i64) -> Result<i64> {
    // Add here check for market price
    let req_string = format!(
        "https://api.yadio.io/convert/{}/{}/BTC",
        fiat_amount, fiat_code
    );
    let req = reqwest::get(req_string)
        .await
        .context("Something went wrong with API request, try again!")?
        .json::<Yadio>()
        .await
        .context("Wrong JSON parse of the answer, check the currency")?;

    let mut sats = req.result * 100_000_000_f64;

    // Added premium value to have correct sats value
    if *premium != 0 {
        sats += (*premium as f64) / 100_f64 * sats;
    }

    Ok(sats as i64)
}

pub async fn publish_order(
    pool: &SqlitePool,
    client: &Client,
    keys: &Keys,
    new_order: &NewOrder,
    initiator_pubkey: &str,
    master_pubkey: &str,
) -> Result<()> {
    let order = crate::db::add_order(pool, new_order, "", initiator_pubkey, master_pubkey).await?;
    let order_id = order.id;
    info!("New order saved Id: {}", order_id);
    // Now we have the order id, we can create a new event adding this id to the Order object
    let order = NewOrder::new(
        Some(order_id),
        OrderKind::from_str(&order.kind).unwrap(),
        Status::Pending,
        order.amount,
        order.fiat_code,
        order.fiat_amount,
        order.payment_method,
        order.premium,
        None,
        None,
        None,
        Some(order.created_at),
    );

    let order_string = order.as_json().unwrap();
    info!("serialized order: {order_string}");
    // This tag (nip33) allows us to change this event in particular in the future
    let event_kind = 30000;
    let d_tag = Tag::Generic(TagKind::Custom("d".to_string()), vec![order_id.to_string()]);
    let event = EventBuilder::new(Kind::Custom(event_kind as u64), &order_string, &[d_tag])
        .to_event(keys)
        .unwrap();
    let event_id = event.id.to_string();
    info!("Publishing Event Id: {event_id} for Order Id: {order_id}");
    // We update the order id with the new event_id
    crate::db::update_order_event_id_status(
        pool,
        order_id,
        &Status::Pending,
        &event_id,
        order.amount,
    )
    .await?;
    client
        .send_event(event)
        .await
        .map(|_s| ())
        .map_err(|err| err.into())
}

pub async fn send_dm(
    client: &Client,
    sender_keys: &Keys,
    receiver_pubkey: &XOnlyPublicKey,
    content: String,
) -> Result<()> {
    info!("DM content: {content:#?}");
    let event = EventBuilder::new_encrypted_direct_msg(sender_keys, *receiver_pubkey, content)?
        .to_event(sender_keys)?;
    info!("Sending event: {event:#?}");
    client.send_event(event).await?;

    Ok(())
}

pub fn get_keys() -> Result<Keys> {
    // nostr private key
    let nsec1privkey = var("NSEC_PRIVKEY").expect("NSEC_PRIVKEY is not set");
    let my_keys = Keys::from_sk_str(&nsec1privkey)?;

    Ok(my_keys)
}

pub async fn update_user_vote_event(user : &String ,buyer_vote : bool, seller_vote : bool, reputation : String , order_id : Uuid, keys :&Keys, client: &Client , pool : &SqlitePool) -> Result<()>{
    // let reputation = reput
    // nip33 kind and d tag
    let event_kind = 30000;
    let d_tag = Tag::Generic(TagKind::Custom("d".to_string()), vec![user.to_string()]);
    let event =
        EventBuilder::new(Kind::Custom(event_kind), reputation, &[d_tag]).to_event(keys)?;
    info!("Sending replaceable event: {event:#?}");
    // We update the order vote status
    crate::db::update_order_event_id_vote_status(pool,order_id, buyer_vote, seller_vote).await?;
    // info!(
    //     "Order Id: {} updated Nostr new Status: {}",
    //     order.id, status_str
    // );

    client.send_event(event).await.map(|_s| ()).map_err(|err| {
        error!("{}", err);
        err.into()
    })
}

pub async fn update_order_event(
    pool: &SqlitePool,
    client: &Client,
    keys: &Keys,
    status: Status,
    order: &Order,
    amount: Option<i64>,
) -> Result<()> {
    let kind = OrderKind::from_str(&order.kind).unwrap();
    let amount = amount.unwrap_or(order.amount);
    let publish_order = NewOrder::new(
        Some(order.id),
        kind,
        status,
        amount,
        order.fiat_code.to_owned(),
        order.fiat_amount,
        order.payment_method.to_owned(),
        order.premium,
        None,
        None,
        None,
        Some(order.created_at),
    );
    let order_string = publish_order.as_json()?;
    // nip33 kind and d tag
    let event_kind = 30000;
    let d_tag = Tag::Generic(TagKind::Custom("d".to_string()), vec![order.id.to_string()]);
    let event =
        EventBuilder::new(Kind::Custom(event_kind), &order_string, &[d_tag]).to_event(keys)?;
    let event_id = event.id.to_string();
    let status_str = status.to_string();
    info!("Sending replaceable event: {event:#?}");
    // We update the order id with the new event_id
    crate::db::update_order_event_id_status(pool, order.id, &status, &event_id, amount).await?;
    info!(
        "Order Id: {} updated Nostr new Status: {}",
        order.id, status_str
    );

    client.send_event(event).await.map(|_s| ()).map_err(|err| {
        error!("{}", err);
        err.into()
    })
}

pub async fn connect_nostr() -> Result<Client> {
    let my_keys = crate::util::get_keys()?;

    // Create new client
    let client = Client::new(&my_keys);
    let relays = var("RELAYS").expect("RELAYS is not set");
    let relays = relays.split(',').collect::<Vec<&str>>();

    // Add relays
    for r in relays.into_iter() {
        client.add_relay(r, None).await?;
    }

    // Connect to relays and keep connection alive
    client.connect().await;

    Ok(client)
}

pub async fn show_hold_invoice(
    pool: &SqlitePool,
    client: &Client,
    my_keys: &Keys,
    payment_request: Option<String>,
    buyer_pubkey: &XOnlyPublicKey,
    seller_pubkey: &XOnlyPublicKey,
    order: &Order,
) -> anyhow::Result<()> {
    let mut ln_client = lightning::LndConnector::new().await;
    // Now we generate the hold invoice that seller should pay
    let (invoice_response, preimage, hash) = ln_client
        .create_hold_invoice(
            &messages::hold_invoice_description(
                my_keys.public_key(),
                &order.id.to_string(),
                &order.fiat_code,
                &order.fiat_amount.to_string(),
            )?,
            order.amount,
        )
        .await?;
    if let Some(invoice) = payment_request {
        db::edit_buyer_invoice_order(pool, order.id, &invoice).await?;
    };

    db::edit_order(
        pool,
        // &Status::WaitingPayment,
        &Status::Success,
        order.id,
        buyer_pubkey,
        seller_pubkey,
        &preimage.to_hex(),
        &hash.to_hex(),
    )
    .await?;
    // We need to publish a new event with the new status
    update_order_event(pool, client, my_keys, Status::Success, order, None).await?;
    let new_order = order.as_new_order();
    // We create a Message to send the hold invoice to seller
    let message = Message::new(
        0,
        Some(order.id),
        None,
        Action::PayInvoice,
        Some(Content::PaymentRequest(
            Some(new_order),
            invoice_response.payment_request,
        )),
    );
    let message = message.as_json()?;

    // We send the hold invoice to the seller
    send_dm(client, my_keys, seller_pubkey, message).await?;

    let message = Message::new(0, Some(order.id), None, Action::WaitingSellerToPay, None);
    let message = message.as_json()?;

    // We send a message to buyer to know that seller was requested to pay the invoice
    send_dm(client, my_keys, buyer_pubkey, message).await?;
    let mut ln_client_invoices = lightning::LndConnector::new().await;
    let (tx, mut rx) = channel(100);

    let invoice_task = {
        async move {
            ln_client_invoices.subscribe_invoice(hash, tx).await;
        }
    };
    tokio::spawn(invoice_task);
    let subs = {
        async move {
            // Receiving msgs from the invoice subscription.
            while let Some(msg) = rx.recv().await {
                let hash = msg.hash.to_hex();
                // If this invoice was paid by the seller
                if msg.state == InvoiceState::Accepted {
                    flow::hold_invoice_paid(&hash).await;
                    println!("Invoice with hash {hash} accepted!");
                } else if msg.state == InvoiceState::Settled {
                    // If the payment was released by the seller
                    println!("Invoice with hash {hash} settled!");
                    flow::hold_invoice_settlement(&hash).await;
                } else if msg.state == InvoiceState::Canceled {
                    // If the payment was canceled
                    println!("Invoice with hash {hash} canceled!");
                    flow::hold_invoice_canceled(&hash).await;
                } else {
                    info!("Invoice with hash: {hash} subscribed!");
                }
            }
        }
    };
    tokio::spawn(subs);

    Ok(())
}

pub async fn set_market_order_sats_amount(
    order: &mut Order,
    buyer_pubkey: XOnlyPublicKey,
    my_keys: &Keys,
    pool: &SqlitePool,
    client: &Client,
) -> Result<i64> {
    // Update amount order
    let new_sats_amout =
        get_market_quote(&order.fiat_amount, &order.fiat_code, &order.premium).await?;
    // We send this data related to the order to the parties
    let order_data = SmallOrder::new(
        order.id,
        new_sats_amout,
        order.fiat_code.clone(),
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        None,
        None,
    );
    // We create a Message
    let message = Message::new(
        0,
        Some(order.id),
        None,
        Action::TakeSell,
        Some(Content::SmallOrder(order_data)),
    );
    let message = message.as_json()?;

    send_dm(client, my_keys, &buyer_pubkey, message).await?;

    // Update order with new sats value
    order.amount = new_sats_amout;
    update_order_event(
        pool,
        client,
        my_keys,
        Status::WaitingBuyerInvoice,
        order,
        None,
    )
    .await?;

    Ok(order.amount)
}

/// Send message to buyer and seller to vote for counterpart
pub async fn vote_counterpart(client : &Client, buyer_pubkey: &XOnlyPublicKey, seller_pubkey: &XOnlyPublicKey, my_keys :&Keys, order : NewOrder) -> Result<()>{

    // Send dm to counterparts
    // to buyer
    let message_to_buyer = Message::new(0, order.id, Action::VoteUser, Some(Content::Order(order.clone())));
    let message_to_buyer = message_to_buyer.as_json().unwrap();
    send_dm(client, my_keys, buyer_pubkey, message_to_buyer).await?;
    // to seller
    let message_to_seller = Message::new(0, order.id, Action::VoteUser, Some(Content::Order(order.clone())));
    let message_to_seller = message_to_seller.as_json().unwrap();
    send_dm(client, my_keys, seller_pubkey, message_to_seller).await?;

    Ok(())
}