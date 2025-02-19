use crate::lightning::LnStatus;
use crate::Settings;
use chrono::Duration;
use mostro_core::order::{Order, Status};
use mostro_core::NOSTR_REPLACEABLE_EVENT_KIND;
use nostr::event::builder::Error;
use nostr_sdk::prelude::*;
use serde_json::json;
use std::borrow::Cow;
use std::vec;

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
    extra_tags: Tags,
) -> Result<Event, Error> {
    let mut tags: Vec<Tag> = Vec::with_capacity(1 + extra_tags.len());
    tags.push(Tag::identifier(identifier));
    tags.extend(extra_tags);
    let tags = Tags::new(tags);

    EventBuilder::new(Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND), content)
        .tags(tags)
        .sign_with_keys(keys)
}

/// Create a rating tag
///
/// # Arguments
///
/// * `reputation_data` - The reputation data of the user
///
/// # Returns a json string
fn create_rating_tag(reputation_data: Option<(f64, i64, i64)>) -> String {
    let now = Timestamp::now();
    let days = (now.as_u64() - reputation_data.map_or(0, |data| data.2) as u64) / 86400;

    let json_data = json!([
        "rating",
        {"total_reviews": reputation_data.map_or(0, |data| data.1), "total_rating": reputation_data.map_or(0.0, |data| data.0), "days": days}
    ]);
    json_data.to_string()
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
pub fn order_to_tags(order: &Order, reputation_data: Option<(f64, i64, i64)>) -> Tags {
    let tags: Vec<Tag> = vec![
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("k")),
            vec![order.kind.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("f")),
            vec![order.fiat_code.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("s")),
            vec![order.status.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("amt")),
            vec![order.amount.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("fa")),
            create_fiat_amt_array(order),
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("pm")),
            vec![order.payment_method.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("premium")),
            vec![order.premium.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("rating")),
            vec![create_rating_tag(reputation_data)],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("network")),
            vec!["mainnet".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("layer")),
            vec!["lightning".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("expiration")),
            vec![(order.expires_at + Duration::hours(12).num_seconds()).to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("y")),
            vec!["mostro".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("z")),
            vec!["order".to_string()],
        ),
    ];

    Tags::new(tags)
}

/// Transform mostro info fields to tags
///
/// # Arguments
///
///
pub fn info_to_tags(ln_status: &LnStatus) -> Tags {
    let mostro_settings = Settings::get_mostro();
    let ln_settings = Settings::get_ln();

    let tags: Tags = Tags::new(vec![
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("mostro_version")),
            vec![env!("CARGO_PKG_VERSION").to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("mostro_commit_hash")),
            vec![env!("GIT_HASH").to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("max_order_amount")),
            vec![mostro_settings.max_order_amount.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("min_order_amount")),
            vec![mostro_settings.min_payment_amount.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("expiration_hours")),
            vec![mostro_settings.expiration_hours.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("expiration_seconds")),
            vec![mostro_settings.expiration_seconds.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("fee")),
            vec![mostro_settings.fee.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("pow")),
            vec![mostro_settings.pow.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("hold_invoice_expiration_window")),
            vec![ln_settings.hold_invoice_expiration_window.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("hold_invoice_cltv_delta")),
            vec![ln_settings.hold_invoice_cltv_delta.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("invoice_expiration_window")),
            vec![ln_settings.hold_invoice_expiration_window.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("lnd_version")),
            vec![ln_status.version.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("lnd_node_pubkey")),
            vec![ln_status.node_pubkey.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("lnd_commit_hash")),
            vec![ln_status.commit_hash.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("lnd_node_alias")),
            vec![ln_status.node_alias.to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("lnd_chains")),
            vec![ln_status.chains.join(",")],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("lnd_networks")),
            vec![ln_status.networks.join(",")],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("lnd_uris")),
            vec![ln_status.uris.join(",")],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("y")),
            vec!["mostrop2p".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("z")),
            vec!["info".to_string()],
        ),
    ]);

    tags
}
