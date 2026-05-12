use crate::app::context::AppContext;
use crate::util::{enqueue_order_msg, get_order};

use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;

pub async fn trade_pubkey_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let mut order = get_order(&msg, pool).await?;

    // Phase 1.5: accept both `Pending` and `WaitingTakerBond` as
    // pre-trade entry points. The trade pubkey is a maker-only piece of
    // state, unaffected by which (if any) prospective taker happens to
    // be mid-bond; gating on `Pending` alone would block legitimate
    // maker rotation during the bond window.
    if order.check_status(Status::Pending).is_err()
        && order.check_status(Status::WaitingTakerBond).is_err()
    {
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

    // Get master keys (already plaintext after phase-3/4 encryption migration)
    let (master_buyer_key, master_seller_key) = if order.master_buyer_pubkey.is_some() {
        let master_buyer_key = order
            .get_master_buyer_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
        (Some(master_buyer_key), None)
    } else {
        let master_seller_key = order
            .get_master_seller_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
        (None, Some(master_seller_key))
    };

    match (master_buyer_key, master_seller_key) {
        (Some(master_buyer_pubkey), _) if master_buyer_pubkey == event.identity.to_string() => {
            order.buyer_pubkey = Some(event.sender.to_string());
        }
        (_, Some(master_seller_pubkey)) if master_seller_pubkey == event.identity.to_string() => {
            order.seller_pubkey = Some(event.sender.to_string());
        }
        _ => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };
    order.creator_pubkey = event.sender.to_string();

    // We a message to the seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::TradePubkey,
        None,
        event.sender,
        None,
    )
    .await;

    order
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}
