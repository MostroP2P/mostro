use crate::db::{
    edit_buyer_pubkey_order, edit_master_buyer_pubkey_order, edit_master_seller_pubkey_order,
    edit_seller_pubkey_order, update_order_to_initial_state,
};
use crate::lightning::LndConnector;
use crate::util::{enqueue_order_msg, get_order, update_order_event};

use mostro_core::error::{
    CantDoReason,
    MostroError::{self, *},
    ServiceError,
};
use mostro_core::message::{Action, Message};
use mostro_core::order::{Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::info;

pub async fn cancel_cooperative_execution(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    request_id: Option<u64>,
    order: &mut Order,
    counterparty_pubkey: String,
    my_keys: &Keys,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    if let Some(initiator) = &order.cancel_initiator_pubkey {
        if *initiator == event.rumor.pubkey.to_string() {
            // We create a Message
            return Err(MostroCantDo(CantDoReason::InvalidPubkey));
        }
    } else {
        if let Some(hash) = &order.hash {
            // We return funds to seller
            ln_client.cancel_hold_invoice(hash).await?;
            info!(
                "Cooperative cancel: Order Id {}: Funds returned to seller",
                &order.id
            );
        }
        order.status = Status::CooperativelyCanceled.to_string();
        // update db
        let order = order
            .clone()
            .update(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        update_order_event(my_keys, Status::CooperativelyCanceled, &order)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
        // We create a Message for an accepted cooperative cancel and send it to both parties
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::CooperativeCancelAccepted,
            None,
            event.rumor.pubkey,
            None,
        )
        .await;
        let counterparty_pubkey = PublicKey::from_str(&counterparty_pubkey)
            .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;
        enqueue_order_msg(
            None,
            Some(order.id),
            Action::CooperativeCancelAccepted,
            None,
            counterparty_pubkey,
            None,
        )
        .await;
        info!("Cancel: Order Id {} canceled cooperatively!", order.id);
    }

    Ok(())
}

pub async fn cancel_order_by_taker(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    order: &mut Order,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    if let Some(hash) = &order.hash {
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }

    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    //We notify the creator that the order was cancelled only if the taker had already done his part before
    if (order.is_sell_order().is_ok() && order.check_status(Status::WaitingPayment).is_ok())
        || (order.is_buy_order().is_ok() && order.check_status(Status::WaitingBuyerInvoice).is_ok())
    {
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::Canceled,
            None,
            order.get_creator_pubkey().map_err(MostroInternalErr)?,
            None,
        )
        .await;

        if order.price_from_api {
            order.amount = 0;
            order.fee = 0;
        }
        if order.is_buy_order().is_ok() {
            edit_seller_pubkey_order(pool, order.id, None)
                .await
                .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
            edit_master_seller_pubkey_order(pool, order.id, None)
                .await
                .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        }
        if order.is_sell_order().is_ok() {
            edit_buyer_pubkey_order(pool, order.id, None)
                .await
                .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
            edit_master_buyer_pubkey_order(pool, order.id, None)
                .await
                .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        }
        update_order_to_initial_state(pool, order.id, order.amount, order.fee)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        update_order_event(my_keys, Status::Pending, order)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
        info!(
            "{}: Canceled order Id {} republishing order",
            buyer_pubkey, order.id
        );
    }

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Canceled,
        None,
        event.rumor.pubkey,
        None,
    )
    .await;

    Ok(())
}

pub async fn cancel_order_by_maker(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    order: &mut Order,
    taker_pubkey: PublicKey,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    if let Ok(order_updated) = update_order_event(my_keys, Status::Canceled, order).await {
        order_updated
            .update(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    }

    if let Some(hash) = &order.hash {
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Canceled,
        None,
        event.rumor.pubkey,
        None,
    )
    .await;
    //We notify the taker that the order was cancelled
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::Canceled,
        None,
        taker_pubkey,
        None,
    )
    .await;

    Ok(())
}

pub async fn cancel_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order id
    let mut order = get_order(&msg, pool).await?;

    if order.check_status(Status::Canceled).is_ok()
        || order.check_status(Status::CooperativelyCanceled).is_ok()
        || order.check_status(Status::CanceledByAdmin).is_ok()
    {
        return Err(MostroCantDo(CantDoReason::OrderAlreadyCanceled));
    }

    if order.check_status(Status::Pending).is_ok() {
        // Validates if this user is the order creator
        order
            .sent_from_maker(event.rumor.pubkey)
            .map_err(|_| MostroCantDo(CantDoReason::IsNotYourOrder))?;
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        if let Ok(order_updated) = update_order_event(my_keys, Status::Canceled, &order).await {
            let _ = order_updated.update(pool).await;
        }
        // We create a Message for cancel
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::Canceled,
            None,
            event.rumor.pubkey,
            None,
        )
        .await;
        return Ok(());
    }

    // Get seller and buyer pubkey
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    if order.check_status(Status::WaitingPayment).is_ok()
        || order.check_status(Status::WaitingBuyerInvoice).is_ok()
    {
        // Get order taker pubkey
        let taker_pubkey = if order.creator_pubkey == seller_pubkey.to_string() {
            buyer_pubkey
        } else if order.creator_pubkey == buyer_pubkey.to_string() {
            seller_pubkey
        } else {
            return Err(MostroInternalErr(ServiceError::InvalidPubkey));
        };

        if order.sent_from_maker(event.rumor.pubkey).is_ok() {
            cancel_order_by_maker(
                pool,
                event,
                &mut order,
                taker_pubkey,
                my_keys,
                request_id,
                ln_client,
            )
            .await?;
        } else if event.rumor.pubkey == taker_pubkey {
            cancel_order_by_taker(pool, event, &mut order, my_keys, request_id, ln_client).await?;
        } else {
            return Err(MostroCantDo(CantDoReason::InvalidPubkey));
        }
    }

    if order.check_status(Status::Active).is_ok()
        || order.check_status(Status::FiatSent).is_ok()
        || order.check_status(Status::Dispute).is_ok()
    {
        let counterparty_pubkey: String;
        if buyer_pubkey == event.rumor.pubkey {
            order.buyer_cooperativecancel = true;
            counterparty_pubkey = seller_pubkey.to_string();
        } else {
            order.seller_cooperativecancel = true;
            counterparty_pubkey = buyer_pubkey.to_string();
        }

        match order.cancel_initiator_pubkey {
            Some(_) => {
                cancel_cooperative_execution(
                    pool,
                    event,
                    request_id,
                    &mut order,
                    counterparty_pubkey,
                    my_keys,
                    ln_client,
                )
                .await?;
            }
            None => {
                order.cancel_initiator_pubkey = Some(event.rumor.pubkey.to_string());
                // update db
                let order = order
                    .update(pool)
                    .await
                    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
                // We create a Message to start a cooperative cancel and send it to both parties
                enqueue_order_msg(
                    request_id,
                    Some(order.id),
                    Action::CooperativeCancelInitiatedByYou,
                    None,
                    event.rumor.pubkey,
                    None,
                )
                .await;
                let counterparty_pubkey = PublicKey::from_str(&counterparty_pubkey)
                    .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;
                enqueue_order_msg(
                    None,
                    Some(order.id),
                    Action::CooperativeCancelInitiatedByPeer,
                    None,
                    counterparty_pubkey,
                    None,
                )
                .await;
            }
        }
    }
    Ok(())
}
