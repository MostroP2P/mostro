use crate::lightning;
use crate::lightning::LndConnector;
use crate::messages;
use crate::models::Yadio;
use crate::{db, flow, RATE_EVENT_LIST};
use anyhow::{Context, Ok, Result};
use dotenvy::var;
use log::{error, info};
use mostro_core::order::{NewOrder, Order, SmallOrder};
use mostro_core::{Action, Content, Kind as OrderKind, Message, Status};
use nostr_sdk::prelude::hex::ToHex;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
use sqlx::{Pool, Sqlite};
use std::str::FromStr;
use tokio::sync::mpsc::channel;
use tonic_openssl_lnd::lnrpc::invoice::InvoiceState;
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

#[allow(clippy::too_many_arguments)]
pub async fn update_user_rating_event(
    user: &String,
    buyer_sent_rate: bool,
    seller_sent_rate: bool,
    reputation: String,
    order_id: Uuid,
    keys: &Keys,
    pool: &SqlitePool,
) -> Result<()> {
    // let reputation = reput
    // nip33 kind and d tag
    let event_kind = 30000;
    let d_tag = Tag::Generic(TagKind::Custom("d".to_string()), vec![user.to_string()]);
    let event = EventBuilder::new(Kind::Custom(event_kind), reputation, &[d_tag]).to_event(keys)?;
    info!("Sending replaceable event: {event:#?}");
    // We update the order vote status
    if buyer_sent_rate {
        crate::db::update_order_event_buyer_rate(pool, order_id, buyer_sent_rate).await?;
    }
    if seller_sent_rate {
        crate::db::update_order_event_seller_rate(pool, order_id, seller_sent_rate).await?;
    }

    // Add event message to global list
    RATE_EVENT_LIST.lock().await.push(event);

    Ok(())
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
    // Add fee of seller to hold invoice
    let seller_fee = var("FEE").unwrap().parse::<f64>().unwrap_or(0.003) / 2.0;
    let seller_total_amount = (seller_fee * order.amount as f64) + order.amount as f64;

    // Now we generate the hold invoice that seller should pay
    let (invoice_response, preimage, hash) = ln_client
        .create_hold_invoice(
            &messages::hold_invoice_description(
                my_keys.public_key(),
                &order.id.to_string(),
                &order.fiat_code,
                &order.fiat_amount.to_string(),
            )?,
            seller_total_amount as i64,
        )
        .await?;
    if let Some(invoice) = payment_request {
        db::edit_buyer_invoice_order(pool, order.id, &invoice).await?;
    };

    db::edit_order(
        pool,
        &Status::WaitingPayment,
        order.id,
        buyer_pubkey,
        seller_pubkey,
        &preimage.to_hex(),
        &hash.to_hex(),
    )
    .await?;
    // We need to publish a new event with the new status
    update_order_event(pool, client, my_keys, Status::WaitingPayment, order, None).await?;
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
                    info!("Invoice with hash {hash} accepted!");
                } else if msg.state == InvoiceState::Settled {
                    // If the payment was settled
                    flow::hold_invoice_settlement(&hash).await;
                } else if msg.state == InvoiceState::Canceled {
                    // If the payment was canceled
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
    let new_sats_amount =
        get_market_quote(&order.fiat_amount, &order.fiat_code, &order.premium).await?;

    // We calculate the bot fee
    let fee = var("FEE").unwrap().parse::<f64>().unwrap() / 2.0;
    let buyer_total_amount = new_sats_amount as f64 - (fee * new_sats_amount as f64);

    // We send this data related to the buyer
    let order_data = SmallOrder::new(
        order.id,
        buyer_total_amount as i64,
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
    order.amount = buyer_total_amount as i64;
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
pub async fn rate_counterpart(
    client: &Client,
    buyer_pubkey: &XOnlyPublicKey,
    seller_pubkey: &XOnlyPublicKey,
    my_keys: &Keys,
    order: NewOrder,
) -> Result<()> {
    // Send dm to counterparts
    // to buyer
    let message_to_buyer = Message::new(0, order.id, None, Action::RateUser, None);
    let message_to_buyer = message_to_buyer.as_json().unwrap();
    send_dm(client, my_keys, buyer_pubkey, message_to_buyer).await?;
    // to seller
    let message_to_seller = Message::new(0, order.id, None, Action::RateUser, None);
    let message_to_seller = message_to_seller.as_json().unwrap();
    send_dm(client, my_keys, seller_pubkey, message_to_seller).await?;

    Ok(())
}

/// Settle a seller hold invoice
#[allow(clippy::too_many_arguments)]
pub async fn settle_seller_hold_invoice(
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
    status: Status,
    action: Action,
    is_admin: bool,
    order: &Order,
) -> Result<()> {
    // It can be settle only by a seller or by admin
    let pubkey = if is_admin {
        my_keys.public_key().to_bech32()?
    } else {
        order.seller_pubkey.as_ref().unwrap().to_string()
    };
    // Check if the pubkey is right
    if event.pubkey.to_bech32()? != pubkey {
        // We create a Message
        let message = Message::new(0, Some(order.id), None, Action::CantDo, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }
    if order.preimage.is_none() {
        return Ok(());
    }

    // Settling the hold invoice
    let preimage = order.preimage.as_ref().unwrap();
    ln_client.settle_hold_invoice(preimage).await?;
    info!("{action}: Order Id {}: hold invoice settled", order.id);
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    update_order_event(pool, client, my_keys, status, order, None).await?;

    Ok(())
}
