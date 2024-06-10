use crate::Settings;
use chrono::Duration;
use mostro_core::order::{Order, Status};
use mostro_core::rating::Rating;
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
    tags.push(Tag::identifier(identifier)); // `d` tag //Check with Yuki!

    for (key, value) in extra_tags.into_iter() {
        let tag = Tag::custom(TagKind::Custom(key.into()), vec![value]);
        tags.push(tag);
    }

    EventBuilder::new(Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND), content, tags).to_event(keys)
}

fn create_fiat_amt_string(order: &Order) -> String {
    if order.min_amount.is_some()
        && order.max_amount.is_some()
        && order.status == Status::Pending.to_string()
    {
        format!(
            "{}-{}",
            order.min_amount.unwrap(),
            order.max_amount.unwrap()
        )
    } else {
        order.fiat_amount.to_string()
    }
}

fn create_rating_string(rating: Option<Rating>) -> String {
    if rating.is_some() {
        format!("{:?}", rating.unwrap(),)
    } else {
        "No user reputation received".to_string()
    }
}

/// Transform an order fields to tags
///
/// # Arguments
///
/// * `order` - The order to transform
///
pub fn order_to_tags(order: &Order, reputation: Option<Rating>) -> Vec<(String, String)> {
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
        ("fa".to_string(), create_fiat_amt_string(order)),
        // payment_method (pm) - The payment method
        ("pm".to_string(), order.payment_method.to_string()),
        // premium (premium) - The premium
        ("premium".to_string(), order.premium.to_string()),
        // User rating
        ("Rating".to_string(), create_rating_string(reputation)),
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

/// Transform mostro info fields to tags
///
/// # Arguments
///
///
pub fn info_to_tags(mostro_pubkey: &PublicKey) -> Vec<(String, String)> {
    let mostro_settings = Settings::get_mostro();
    let ln_settings = Settings::get_ln();

    let tags = vec![
        // max_order_amount
        ("mostro_pubkey".to_string(), mostro_pubkey.to_string()),
        // mostro version
        (
            "mostro_version".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        ),
        // mostro commit id
        ("mostro_commit_id".to_string(), env!("GIT_HASH").to_string()),
        // max_order_amount
        (
            "max_order_amount".to_string(),
            mostro_settings.max_order_amount.to_string(),
        ),
        // min_order_amount
        (
            "min_order_amount".to_string(),
            mostro_settings.min_payment_amount.to_string(),
        ),
        // expiration_hours
        (
            "expiration_hours".to_string(),
            mostro_settings.expiration_hours.to_string(),
        ),
        // expiration_seconds
        (
            "expiration_seconds".to_string(),
            mostro_settings.expiration_seconds.to_string(),
        ),
        // fee
        ("fee".to_string(), mostro_settings.fee.to_string()),
        // hold_invoice_expiration_window
        (
            "hold_invoice_expiration_window".to_string(),
            ln_settings.hold_invoice_expiration_window.to_string(),
        ),
        // hold_invoice_cltv_delta
        (
            "hold_invoice_cltv_delta".to_string(),
            ln_settings.hold_invoice_cltv_delta.to_string(),
        ),
        // invoice_expiration_window
        (
            "invoice_expiration_window".to_string(),
            ln_settings.hold_invoice_expiration_window.to_string(),
        ),
        // Label to identify this is a Mostro's infos
        ("y".to_string(), "mostrop2p".to_string()),
        // Table name
        ("z".to_string(), "info".to_string()),
    ];

    tags
}
