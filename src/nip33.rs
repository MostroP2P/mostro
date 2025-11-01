use crate::config::settings::Settings;
use crate::lightning::LnStatus;
use crate::LN_STATUS;
use chrono::Duration;
use mostro_core::prelude::*;
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
    let tags = Tags::from_list(tags);

    EventBuilder::new(nostr::Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND), content)
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
    if let Some(data) = reputation_data {
        const SECONDS_IN_DAY: u64 = 86400;
        // If operating day is 0, it means the user is new and we don't have a valid reputation data
        let days = if data.2 != 0 {
            let now = Timestamp::now();
            (now.as_u64() - data.2 as u64) / SECONDS_IN_DAY
        } else {
            0
        };

        // Create the json string
        let json_data = json!([
        "rating",
            {"total_reviews": data.1, "total_rating": data.0, "days": days}
        ]);
        json_data.to_string()
    } else {
        "{}".to_string()
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

///
/// # Arguments
///
/// * `order` - the order struct
///
/// # Returns a json string with order status according to nip69
/// Possible states for nostr event are pending, in-progress, success, canceled
fn create_status_tags(order: &Order) -> Result<(bool, Status), MostroError> {
    // Check if the order is pending/in-progress/success/canceled
    let status = order.get_order_status().map_err(MostroInternalErr)?;

    match status {
        Status::WaitingBuyerInvoice => Ok((order.is_sell_order().is_ok(), Status::InProgress)),
        Status::WaitingPayment => Ok((order.is_buy_order().is_ok(), Status::InProgress)),
        Status::Canceled
        | Status::CanceledByAdmin
        | Status::CooperativelyCanceled
        | Status::Expired => Ok((true, Status::Canceled)),
        Status::Success | Status::CompletedByAdmin => Ok((true, status)),
        Status::Pending => Ok((true, status)),
        _ => Ok((false, status)),
    }
}
/// Create a custom source reference for pending orders
///
/// This function generates a source tag containing a custom reference format that allows
/// clients to find and reference the original order event. The source tag is only created
/// for pending orders that need to be discoverable by potential takers.
///
/// # Arguments
///
/// * `order` - The order to create a source tag for
/// * `mostro_relays` - List of relay URLs where the order event can be found
///
/// # Returns
///
/// * `Ok(Some(String))` - If the order is pending, returns a custom reference string
/// * `Ok(None)` - If the order is not pending (source tags only apply to pending orders)
/// * `Err(MostroError)` - If there was an error creating the reference
///
/// # Behavior
///
/// The function only creates source tags for pending orders, as these are the orders that
/// need to be discoverable and referenceable by potential takers. The generated reference
/// includes:
/// - Order ID
/// - List of relays where the event can be found
///
/// The resulting reference uses a custom format: `mostro:{order_id}?{relay1,relay2,...}`
///
fn create_source_tag(
    order: &Order,
    mostro_relays: &[String],
) -> Result<Option<String>, MostroError> {
    if order.status == Status::Pending.to_string() {
        // Create a mostro: custom source reference for pending orders
        let custom_ref = format!("mostro:{}?relays={}", order.id, mostro_relays.join(","));

        Ok(Some(custom_ref))
    } else {
        Ok(None)
    }
}

/// Transform an order into Nostr tags for NIP-33 replaceable events
///
/// This function converts an order's fields into a collection of Nostr tags that can be used
/// to create or update a NIP-33 replaceable event. The function handles the complete lifecycle
/// of an order, from pending to completion or cancellation, and creates appropriate tags
/// for each status.
///
/// # Arguments
///
/// * `order` - The order to transform into tags
/// * `reputation_data` - Optional reputation data tuple containing:
///   - `f64`: Total rating score
///   - `i64`: Total number of reviews
///   - `i64`: Unix timestamp of first operation (used to calculate operating days)
///
/// # Returns
///
/// * `Ok(Some(Tags))` - If the order should be published as a Nostr event with the generated tags
/// * `Ok(None)` - If the order should not be published (e.g., certain internal statuses)
/// * `Err(MostroError)` - If there was an error processing the order or creating tags
///
/// # Behavior
///
/// The function creates tags following NIP-69 specifications for peer-to-peer marketplaces:
/// - `k`: Order kind (buy/sell)
/// - `f`: Fiat currency code
/// - `s`: Order status (pending/in-progress/success/canceled)
/// - `amt`: Bitcoin amount in satoshis
/// - `fa`: Fiat amount array (min/max for pending orders, exact for others)
/// - `pm`: Payment methods (comma-separated)
/// - `premium`: Premium percentage
/// - `network`: Lightning network
/// - `layer`: Always "lightning"
/// - `expiration`: Order expiration timestamp
/// - `y`: Always "mostro" (marketplace identifier)
/// - `z`: Always "order" (event type)
/// - `rating`: User reputation data (if available)
/// - `source`: mostro: scheme link to pending orders (`mostro:{order_id}?{relay1,relay2,...}`)
///
pub fn order_to_tags(
    order: &Order,
    reputation_data: Option<(f64, i64, i64)>,
) -> Result<Option<Tags>, MostroError> {
    // Position of the tags in the list
    const RATING_TAG_INDEX: usize = 7;
    const SOURCE_TAG_INDEX: usize = 8;

    // Check if the order is pending/in-progress/success/canceled
    let (create_event, status) = create_status_tags(order)?;
    // Create mostro: scheme link in case of pending order creation
    let mostro_link = create_source_tag(order, &Settings::get_nostr().relays)?;

    // Send just in case the order is pending/in-progress/success/canceled
    if create_event {
        let ln_network = match LN_STATUS.get() {
            Some(status) => status.networks.join(","),
            None => "unknown".to_string(),
        };
        let payment_method: Vec<String> = order
            .payment_method
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        let mut tags: Vec<Tag> = vec![
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
                vec![status.to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("amt")),
                vec![order.amount.to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("fa")),
                create_fiat_amt_array(order),
            ),
            Tag::custom(TagKind::Custom(Cow::Borrowed("pm")), payment_method),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("premium")),
                vec![order.premium.to_string()],
            ),
            Tag::custom(TagKind::Custom(Cow::Borrowed("network")), vec![ln_network]),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("layer")),
                vec!["lightning".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("order_expires_at")),
                vec![order.expires_at.to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("expiration")),
                vec![(order.expires_at + Duration::hours(24).num_seconds()).to_string()],
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

        // Add reputation data if available
        if reputation_data.is_some() {
            tags.insert(
                RATING_TAG_INDEX,
                Tag::custom(
                    TagKind::Custom(Cow::Borrowed("rating")),
                    vec![create_rating_tag(reputation_data)],
                ),
            );
        }
        // Add source tag if available
        if let Some(source) = mostro_link {
            tags.insert(
                SOURCE_TAG_INDEX,
                Tag::custom(TagKind::Custom(Cow::Borrowed("source")), vec![source]),
            );
        }
        Ok(Some(Tags::from_list(tags)))
    } else {
        Ok(None)
    }
}

/// Transform mostro info fields to tags
///
/// # Arguments
///
///
pub fn info_to_tags(ln_status: &LnStatus) -> Tags {
    let mostro_settings = Settings::get_mostro();
    let ln_settings = Settings::get_ln();

    let tags: Tags = Tags::from_list(vec![
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
            TagKind::Custom(Cow::Borrowed("fiat_currencies_accepted")),
            vec![mostro_settings.fiat_currencies_accepted.join(",")],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("max_orders_per_response")),
            vec![mostro_settings.max_orders_per_response.to_string()],
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
            vec!["mostro".to_string()],
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("z")),
            vec!["info".to_string()],
        ),
    ]);

    tags
}
