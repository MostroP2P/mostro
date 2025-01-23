use crate::db::{edit_buyer_pubkey_order, edit_seller_pubkey_order, update_order_to_initial_state};
use crate::lightning::LndConnector;
use crate::util::{enqueue_order_msg, get_order, update_order_event};

use anyhow::Result;
use mostro_core::error::{
    CantDoReason,
    MostroError::{self, *},
    ServiceError,
};
use mostro_core::message::{Action, Message};
use mostro_core::order::{Kind as OrderKind, Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::info;

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

    let user_pubkey = event.rumor.pubkey.to_string();

    if order.check_status(Status::Canceled).is_ok()
        || order.check_status(Status::CooperativelyCanceled).is_ok()
        || order.check_status(Status::CanceledByAdmin).is_ok()
    {
        return Err(MostroCantDo(CantDoReason::OrderAlreadyCanceled));
    }

    if order.check_status(Status::Pending).is_ok() {
        // Validates if this user is the order creator
        if order.sent_from_maker(user_pubkey).is_err() {
            return Err(MostroCantDo(CantDoReason::IsNotYourOrder));
        } else {
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
        }

        return Ok(());
    }

    if order.kind == OrderKind::Sell.to_string()
        && order.status == Status::WaitingBuyerInvoice.to_string()
    {
        cancel_add_invoice(ln_client, &mut order, event, pool, my_keys, request_id).await?;
    }

    if order.kind == OrderKind::Buy.to_string()
        && order.status == Status::WaitingPayment.to_string()
    {
        cancel_pay_hold_invoice(ln_client, &mut order, event, pool, my_keys, request_id).await?;
    }

    if order.status == Status::Active.to_string()
        || order.status == Status::FiatSent.to_string()
        || order.status == Status::Dispute.to_string()
    {
        let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
            (Some(seller), Some(buyer)) => (seller, buyer),
            (None, _) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
            (_, None) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
        };

        let counterparty_pubkey: String;
        if buyer_pubkey == &user_pubkey {
            order.buyer_cooperativecancel = true;
            counterparty_pubkey = seller_pubkey.to_string();
        } else {
            order.seller_cooperativecancel = true;
            counterparty_pubkey = buyer_pubkey.to_string();
        }

        match order.cancel_initiator_pubkey {
            Some(ref initiator_pubkey) => {
                if initiator_pubkey == &user_pubkey {
                    // We create a Message
                    return Err(MostroCantDo(CantDoReason::InvalidPubkey));
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
                    let order = order.update(pool).await.map_err(|e| {
                        MostroInternalErr(ServiceError::DbAccessError(e.to_string()))
                    })?;
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
            }
            None => {
                order.cancel_initiator_pubkey = Some(user_pubkey.clone());
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

pub async fn cancel_add_invoice(
    ln_client: &mut LndConnector,
    order: &mut Order,
    event: &UnwrappedGift,
    pool: &Pool<Sqlite>,
    my_keys: &Keys,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    if let Some(hash) = &order.hash {
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }

    let user_pubkey = event.rumor.pubkey.to_string();

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            buyer,
        ),
        (None, _) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
        (_, None) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };

    if buyer_pubkey != &user_pubkey {
        // We create a Message
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    if &order.creator_pubkey == buyer_pubkey {
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        update_order_event(my_keys, Status::CooperativelyCanceled, order)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
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
        enqueue_order_msg(
            None,
            Some(order.id),
            Action::Canceled,
            None,
            seller_pubkey,
            None,
        )
        .await;
        Ok(())
    } else {
        // We re-publish the event with Pending status
        // and update on local database
        if order.price_from_api {
            order.amount = 0;
            order.fee = 0;
        }
        edit_buyer_pubkey_order(pool, order.id, None)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
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
        Ok(())
    }
}

pub async fn cancel_pay_hold_invoice(
    ln_client: &mut LndConnector,
    order: &mut Order,
    event: &UnwrappedGift,
    pool: &Pool<Sqlite>,
    my_keys: &Keys,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    if order.hash.is_some() {
        // We return funds to seller
        if let Some(hash) = order.hash.as_ref() {
            ln_client
                .cancel_hold_invoice(hash)
                .await
                .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?;
            info!("Order Id {}: Funds returned to seller", &order.id);
        }
    }
    let user_pubkey = event.rumor.pubkey.to_string();

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            buyer,
        ),
        (None, _) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
        (_, None) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };

    if seller_pubkey.to_string() != user_pubkey {
        // We create a Message
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    if order.creator_pubkey == seller_pubkey.to_string() {
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        update_order_event(my_keys, Status::Canceled, order)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
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
        enqueue_order_msg(
            None,
            Some(order.id),
            Action::Canceled,
            None,
            seller_pubkey,
            None,
        )
        .await;
        Ok(())
    } else {
        // We re-publish the event with Pending status
        // and update on local database
        if order.price_from_api {
            order.amount = 0;
            order.fee = 0;
        }
        edit_seller_pubkey_order(pool, order.id, None)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        update_order_to_initial_state(pool, order.id, order.amount, order.fee)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        update_order_event(my_keys, Status::Pending, order)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        info!(
            "{}: Canceled order Id {} republishing order",
            buyer_pubkey, order.id
        );
        Ok(())
    }
}
