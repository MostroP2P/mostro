use crate::config::MOSTRO_DB_PASSWORD;
use crate::db::is_user_present;
use crate::util::queues::enqueue_order_msg;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

/// Send message to buyer and seller to vote for counterpart
pub async fn rate_counterpart(
    buyer_pubkey: &PublicKey,
    seller_pubkey: &PublicKey,
    order: &Order,
    request_id: Option<u64>,
) -> Result<()> {
    // Send dm to counterparts
    // to buyer
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Rate,
        None,
        *buyer_pubkey,
        None,
    )
    .await;
    // to seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Rate,
        None,
        *seller_pubkey,
        None,
    )
    .await;

    Ok(())
}

pub async fn notify_taker_reputation(
    pool: &Pool<Sqlite>,
    order: &Order,
) -> Result<(), MostroError> {
    // Check if is buy or sell order we need this info to understand the user needed and the receiver of notification
    let is_buy_order = order.is_buy_order().is_ok();
    // Get user needed
    let user = match is_buy_order {
        true => order.master_seller_pubkey.clone(),
        false => order.master_buyer_pubkey.clone(),
    };

    let user_decrypted_key = if let Some(user) = user {
        // Get reputation data
        CryptoUtils::decrypt_data(user, MOSTRO_DB_PASSWORD.get()).map_err(MostroInternalErr)?
    } else {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    };

    let reputation_data = match is_user_present(pool, user_decrypted_key).await {
        Ok(user) => {
            let now = Timestamp::now().as_u64();
            UserInfo {
                rating: user.total_rating,
                reviews: user.total_reviews,
                operating_days: (now - user.created_at as u64) / 86400,
            }
        }
        Err(_) => UserInfo {
            rating: 0.0,
            reviews: 0,
            operating_days: 0,
        },
    };

    // Get order status
    let order_status = order.get_order_status().map_err(MostroInternalErr)?;

    // Get action for info message and receiver key
    let (action, receiver) = match order_status {
        Status::WaitingBuyerInvoice => {
            if !is_buy_order {
                (
                    Action::PayInvoice,
                    order.get_seller_pubkey().map_err(MostroInternalErr)?,
                )
            } else {
                //FIX for the case of a buy order and maker is adding invoice
                // just return ok
                return Ok(());
            }
        }
        Status::WaitingPayment => {
            if is_buy_order {
                (
                    Action::AddInvoice,
                    order.get_buyer_pubkey().map_err(MostroInternalErr)?,
                )
            } else {
                return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
            }
        }
        _ => {
            return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
        }
    };

    enqueue_order_msg(
        None,
        Some(order.id),
        action,
        Some(Payload::Peer(Peer {
            pubkey: "".to_string(),
            reputation: Some(reputation_data),
        })),
        receiver,
        None,
    )
    .await;
    Ok(())
}

pub async fn get_dispute(msg: &Message, pool: &Pool<Sqlite>) -> Result<Dispute, MostroError> {
    let dispute_msg = msg.get_inner_message_kind();
    let dispute_id = dispute_msg
        .id
        .ok_or(MostroInternalErr(ServiceError::InvalidDisputeId))?;
    let dispute = Dispute::by_id(pool, dispute_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    if let Some(dispute) = dispute {
        Ok(dispute)
    } else {
        Err(MostroInternalErr(ServiceError::InvalidDisputeId))
    }
}
