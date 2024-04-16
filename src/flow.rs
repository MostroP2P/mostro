use crate::util::send_new_order_msg;
use anyhow::{Result,Error};
use mostro_core::message::{Action, Content};
use mostro_core::order::{Kind, SmallOrder, Status};
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::{error, info};

pub async fn hold_invoice_paid(hash: &str) -> Result<()>{
    let pool = crate::db::connect().await?;
    let order = crate::db::find_order_by_hash(&pool, hash).await?;
    let my_keys = crate::util::get_keys()?;

    let (seller_pubkey, buyer_pubkey) = match (order.seller_pubkey, order.buyer_pubkey) {
            (Some(seller), Some(buyer)) => (PublicKey::from_str(seller.as_str())?, PublicKey::from_str(buyer.as_str())?),
            (None, _) => return Err(Error::msg("Missing seller pubkey")),
            (_, None) => return Err(Error::msg("Missing buyer pubkey")),
            _ => return Err(Error::msg("Missing pubkeys")),
    };
    
    info!(
        "Order Id: {} - Seller paid invoice with hash: {hash}",
        order.id
    );

    let order_kind = match Kind::from_str(&order.kind) {
        Ok(k) => k,
        Err(e) => {
            error!("Order Id {} wrong kind: {:?}", order.id, e);
            return Err(e.into());
        }
    };

    // We send this data related to the order to the parties
    let mut order_data = SmallOrder::new(
        Some(order.id),
        Some(order_kind),
        None,
        order.amount,
        order.fiat_code.clone(),
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        order.buyer_pubkey.as_ref().cloned(),
        order.seller_pubkey.as_ref().cloned(),
        None,
        Some(order.created_at),
        Some(order.expires_at),
    );
    let status;

    if order.buyer_invoice.is_some() {
        status = Status::Active;
        order_data.status = Some(status);
        // We send a confirmation message to seller
        send_new_order_msg(
            Some(order.id),
            Action::BuyerTookOrder,
            Some(Content::Order(order_data.clone())),
            &seller_pubkey,
        )
        .await;
        // We send a message to buyer saying seller paid
        send_new_order_msg(
            Some(order.id),
            Action::HoldInvoicePaymentAccepted,
            Some(Content::Order(order_data)),
            &buyer_pubkey,
        )
        .await;
    } else {
        let new_amount = order_data.amount - order.fee;
        order_data.amount = new_amount;
        status = Status::WaitingBuyerInvoice;
        order_data.status = Some(status);
        order_data.master_buyer_pubkey = None;
        order_data.master_seller_pubkey = None;
        // We ask to buyer for a new invoice
        send_new_order_msg(
            Some(order.id),
            Action::AddInvoice,
            Some(Content::Order(order_data)),
            &buyer_pubkey,
        )
        .await;

        // We send a message to seller we are waiting for buyer invoice
        send_new_order_msg(
            Some(order.id),
            Action::WaitingBuyerInvoice,
            None,
            &seller_pubkey,
        )
        .await;
    }
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    if let Ok(updated_order) = crate::util::update_order_event(&my_keys, status, &order).await {
        // Update order on db
        let _ = updated_order.update(&pool).await;
    }

    // Update the invoice_held_at field
    crate::db::update_order_invoice_held_at_time(&pool, order.id, Timestamp::now().as_i64())
        .await?;
        

    Ok(())
}

pub async fn hold_invoice_settlement(hash: &str) -> Result<()>{
    let pool = crate::db::connect().await?;
    let order = crate::db::find_order_by_hash(&pool, hash).await?;
    info!(
        "Order Id: {} - Invoice with hash: {} was settled!",
        order.id, hash
    );
    Ok(())
}

pub async fn hold_invoice_canceled(hash: &str) -> Result<()> {
    let pool = crate::db::connect().await?;
    let order = crate::db::find_order_by_hash(&pool, hash).await?;
    info!(
        "Order Id: {} - Invoice with hash: {} was canceled!",
        order.id, hash
    );
    Ok(())
}
