use crate::config::MESSAGE_QUEUES;
use mostro_core::prelude::*;
use nostr_sdk::prelude::PublicKey;
use uuid::Uuid;

pub async fn enqueue_cant_do_msg(
    request_id: Option<u64>,
    order_id: Option<Uuid>,
    reason: CantDoReason,
    destination_key: PublicKey,
) {
    // Send message to event creator
    let message = Message::cant_do(order_id, request_id, Some(Payload::CantDo(Some(reason))));
    MESSAGE_QUEUES
        .queue_order_cantdo
        .write()
        .await
        .push((message, destination_key));
}

pub async fn enqueue_restore_session_msg(payload: Option<Payload>, destination_key: PublicKey) {
    // Send message to event creator
    let message = Message::new_restore(payload);
    MESSAGE_QUEUES
        .queue_restore_session_msg
        .write()
        .await
        .push((message, destination_key));
}

pub async fn enqueue_order_msg(
    request_id: Option<u64>,
    order_id: Option<Uuid>,
    action: Action,
    payload: Option<Payload>,
    destination_key: PublicKey,
    trade_index: Option<i64>,
) {
    // Send message to event creator
    let message = Message::new_order(order_id, request_id, trade_index, action, payload);
    MESSAGE_QUEUES
        .queue_order_msg
        .write()
        .await
        .push((message, destination_key));
}
