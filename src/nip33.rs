use crate::stats::MostroMessageStats;
use chrono::Duration;
use mostro_core::order::Order;
use mostro_core::NOSTR_REPLACEABLE_EVENT_KIND;
use nostr::event::builder::Error;
use nostr_sdk::prelude::*;

/// Creates a new mostro nip33 event
///
/// # Arguments
///
/// * `keys` - The keys used to sign the event
/// * `content` - The content of the event
/// * `identifier` - The nip33 d tag used to replaced the event with a new one
/// * `extra_tags` - The nip33 other tags used to subscribe order type notifications
///
/// # Returns
/// Returns a new event
///
pub fn new_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Vec<(String, String)>,
) -> Result<Event, Error> {
    // This tag (nip33) allows us to change this event in particular in the future
    let mut tags: Vec<Tag> = Vec::with_capacity(1 + extra_tags.len());
    tags.push(Tag::Identifier(identifier)); // `d` tag
    for (key, value) in extra_tags.into_iter() {
        let tag = Tag::Generic(TagKind::Custom(key), vec![value]);
        tags.push(tag);
    }

    EventBuilder::new(Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND), content, tags).to_event(keys)
}

/// Transform an order fields to tags
///
/// # Arguments
///
/// * `order` - The order to transform
///
pub fn order_to_tags(order: &Order) -> Vec<(String, String)> {
    let tags = vec![
        // kind (k) - The order kind (buy or sell)
        ("k".to_string(), order.kind.to_string()),
        // fiat_code (f) - The fiat code of the order
        ("f".to_string(), order.fiat_code.to_string()),
        // status (s) - The order status
        ("s".to_string(), order.status.to_string()),
        // amount (amt) - The amount of sats
        ("amt".to_string(), order.amount.to_string()),
        // fiat_amount (fa) - The fiat amount
        ("fa".to_string(), order.fiat_amount.to_string()),
        // payment_method (pm) - The payment method
        ("pm".to_string(), order.payment_method.to_string()),
        // premium (premium) - The premium
        ("premium".to_string(), order.premium.to_string()),
        // Label to identify this is a Mostro's order
        ("y".to_string(), "mostrop2p".to_string()),
        // Table name
        ("z".to_string(), "order".to_string()),
        // Nip 40 expiration time - 12 hours over expiration user time
        (
            "expiration".to_string(),
            (order.expires_at + Duration::hours(12).num_seconds()).to_string(),
        ),
    ];

    tags
}

/// Transform stats fields to tags
///
/// # Arguments
///
/// * `stats` - The order to transform
///
pub fn stats_to_tags(stats: &MostroStats) -> Vec<(String, String)> {
    let tags = vec![
        // Total amount of new orders
        ("new_orders".to_string(), stats. new_order.to_string()),
        // Total amount of new disputes
        ("new_disputes".to_string(), stats.new_dispute.to_string()),
        // Total amount of successful orders
        ("completed_orders".to_string(), stats.release.to_string()),
        // amount (amt) - The amount of sats
        // Label to identify this is a Mostro's order
        ("y".to_string(), "mostrop2p".to_string()),
        // Table name
        ("z".to_string(), "overall-stats".to_string()),
    ];

    tags
}
