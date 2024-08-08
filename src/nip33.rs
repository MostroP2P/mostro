use std::vec;

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
    extra_tags: Vec<Tag>,
) -> Result<Event, Error> {
    let mut tags: Vec<Tag> = vec![];
    tags.push(Tag::identifier(identifier));
    tags.append(&mut extra_tags.clone());

    EventBuilder::new(Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND), content, tags).to_event(keys)
}

fn create_rating_string(rating: Option<Rating>) -> String {
    if let Some(rating) = rating {
        if let Ok(rating_json) = rating.as_json() {
            rating_json
        } else {
            "bad format value".to_string()
        }
    } else {
        "none".to_string()
    }
}

fn create_fiat_amt_array(order: &Order) -> Vec<String> {
    if order.status == Status::Pending.to_string() {
        match (order.min_amount, order.max_amount) {
            (Some(min), Some(max)) => {
                vec![min.to_string(), max.to_string()]
            }
            _ => {
                vec![order.fiat_amount.to_string()]
            }
        }
    } else {
        vec![order.fiat_amount.to_string()]
    }
}

/// Transform an order fields to tags
///
/// # Arguments
///
/// * `order` - The order to transform
///
pub fn order_to_tags(order: &Order, reputation: Option<Rating>) -> Vec<Tag> {
    let tags: Vec<Tag> = vec![
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("k")),
            vec![order.kind.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("f")),
            vec![order.fiat_code.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("s")),
            vec![order.status.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("amt")),
            vec![order.amount.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("fa")),
            create_fiat_amt_array(order),
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("pm")),
            vec![order.payment_method.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("premium")),
            vec![order.premium.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("rating")),
            vec![create_rating_string(reputation)],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("network")),
            vec!["mainnet".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("layer")),
            vec!["lightning".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("expiration")),
            vec![(order.expires_at + Duration::hours(12).num_seconds()).to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("y")),
            vec!["mostro".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("z")),
            vec!["order".to_string()],
        ),
    ];

    tags
}

/// Transform mostro info fields to tags
///
/// # Arguments
///
///
pub fn info_to_tags(mostro_pubkey: &PublicKey) -> Vec<Tag> {
    let mostro_settings = Settings::get_mostro();
    let ln_settings = Settings::get_ln();

    let tags: Vec<Tag> = vec![
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("mostro_pubkey")),
            vec![mostro_pubkey.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("mostro_version")),
            vec![env!("CARGO_PKG_VERSION").to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("mostro_commit_id")),
            vec![env!("GIT_HASH").to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("max_order_amount")),
            vec![mostro_settings.max_order_amount.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("min_order_amount")),
            vec![mostro_settings.min_payment_amount.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("expiration_hours")),
            vec![mostro_settings.expiration_hours.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("expiration_seconds")),
            vec![mostro_settings.expiration_seconds.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("fee")),
            vec![mostro_settings.fee.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("pow")),
            vec![mostro_settings.pow.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("hold_invoice_expiration_window")),
            vec![ln_settings.hold_invoice_expiration_window.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("hold_invoice_cltv_delta")),
            vec![ln_settings.hold_invoice_cltv_delta.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("invoice_expiration_window")),
            vec![ln_settings.hold_invoice_expiration_window.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("y")),
            vec!["mostrop2p".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(std::borrow::Cow::Borrowed("z")),
            vec!["info".to_string()],
        ),
    ];

    tags
}
