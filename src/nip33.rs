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
    let d_tag = Tag::Generic(TagKind::Custom("d".to_string()), vec![identifier]);
    let mut tags = vec![d_tag];
    for tag in extra_tags {
        let tag = Tag::Generic(TagKind::Custom(tag.0), vec![tag.1]);
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
    ];

    tags
}
