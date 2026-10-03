//! Shared fixtures for the payer-history handler tests.

use crate::app::context::test_utils::{test_settings, TestContextBuilder};
use crate::app::context::AppContext;
use crate::config::payer_history::PayerHistorySettings;
use crate::config::MESSAGE_QUEUES;
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
use std::sync::Arc;

pub async fn create_test_pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!().run(&pool).await.unwrap();
    pool
}

/// `[payer_history]` with the given switches and default thresholds.
pub fn payer_settings(enabled: bool, require_declaration: bool) -> PayerHistorySettings {
    PayerHistorySettings {
        enabled,
        require_declaration,
        ..Default::default()
    }
}

/// An `AppContext` over `pool` whose settings carry `payer_history`.
pub fn ctx_with(pool: &SqlitePool, payer_history: Option<PayerHistorySettings>) -> AppContext {
    let mut settings = test_settings();
    settings.payer_history = payer_history;
    TestContextBuilder::new()
        .with_pool(Arc::new(pool.clone()))
        .with_settings(settings)
        .build()
}

pub fn hash(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

pub fn unwrapped(sender: PublicKey, msg: Message) -> UnwrappedMessage {
    UnwrappedMessage {
        message: msg,
        signature: None,
        sender,
        identity: Keys::generate().public_key(),
        created_at: Timestamp::now(),
    }
}

/// Trade and identity keys of both parties. A party whose trade key equals
/// its master key is in Full Privacy Mode (`Order::is_full_privacy_order`).
#[derive(Clone, Copy)]
pub struct Parties {
    pub seller: PublicKey,
    pub seller_master: PublicKey,
    pub buyer: PublicKey,
    pub buyer_master: PublicKey,
}

impl Parties {
    /// Both parties in reputation mode (trade key ≠ identity key).
    pub fn reputation() -> Self {
        Self {
            seller: Keys::generate().public_key(),
            seller_master: Keys::generate().public_key(),
            buyer: Keys::generate().public_key(),
            buyer_master: Keys::generate().public_key(),
        }
    }

    /// Like [`Parties::reputation`], with the buyer in Full Privacy Mode.
    pub fn full_privacy_buyer() -> Self {
        let mut p = Self::reputation();
        p.buyer_master = p.buyer;
        p
    }
}

/// A sell order in `status` between `parties`.
pub async fn order_in(pool: &SqlitePool, status: Status, parties: Parties) -> Order {
    Order {
        id: uuid::Uuid::new_v4(),
        status: status.to_string(),
        kind: mostro_core::order::Kind::Sell.to_string(),
        fiat_code: "USD".to_string(),
        creator_pubkey: parties.seller.to_string(),
        seller_pubkey: Some(parties.seller.to_string()),
        master_seller_pubkey: Some(parties.seller_master.to_string()),
        buyer_pubkey: Some(parties.buyer.to_string()),
        master_buyer_pubkey: Some(parties.buyer_master.to_string()),
        amount: 21_000,
        fee: 21,
        fiat_amount: 40,
        ..Default::default()
    }
    .create(pool)
    .await
    .unwrap()
}

/// One message queued for an order.
#[derive(Debug, Clone)]
pub struct Queued {
    pub action: Action,
    pub request_id: Option<u64>,
    pub destination: PublicKey,
    pub payload: Option<Payload>,
}

impl Queued {
    /// The history carried by a `payment-history` message, if any.
    pub fn history(&self) -> Option<&PaymentHistory> {
        match &self.payload {
            Some(Payload::PaymentHistory(h)) => Some(h),
            _ => None,
        }
    }

    /// The hash carried by a `payer-declared` message, if any.
    pub fn declared_hash(&self) -> Option<&str> {
        match &self.payload {
            Some(Payload::PayerDeclaration(d)) => Some(&d.payment_hash),
            _ => None,
        }
    }
}

/// Messages queued for `order_id`, in order. The queue is process-global,
/// so always filter by our own order id.
pub async fn queued_for(order_id: uuid::Uuid) -> Vec<Queued> {
    MESSAGE_QUEUES
        .queue_order_msg
        .read()
        .await
        .iter()
        .filter(|(msg, _)| msg.get_inner_message_kind().id == Some(order_id))
        .map(|(msg, dest)| {
            let k = msg.get_inner_message_kind();
            Queued {
                action: k.action.clone(),
                request_id: k.request_id,
                destination: *dest,
                payload: k.payload.clone(),
            }
        })
        .collect()
}

pub fn assert_cant_do<T: std::fmt::Debug>(res: Result<T, MostroError>, expected: CantDoReason) {
    match res {
        Err(MostroCantDo(reason)) => assert_eq!(reason, expected),
        other => panic!("expected cant-do {expected:?}, got {other:?}"),
    }
}
