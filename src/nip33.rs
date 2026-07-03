use crate::config::constants::NOSTR_EXCHANGE_RATES_EVENT_KIND;
use crate::config::settings::Settings;
use crate::config::types::BondApplyTo;
use crate::lightning::LnStatus;
use crate::util::{get_expiration_timestamp_for_kind, get_keys};
use crate::LN_STATUS;
use mostro_core::prelude::*;
use nostr::event::builder::Error;
use nostr_sdk::prelude::*;
use serde_json::json;
use std::borrow::Cow;
use std::vec;

/// Internal helper function to create a NIP-33 replaceable event with a specific kind
fn create_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
    kind: u16,
) -> Result<Event, Error> {
    let mut tags: Vec<Tag> = Vec::with_capacity(2 + extra_tags.len());
    tags.push(Tag::identifier(identifier));

    // Add NIP-40 expiration tag if configured and not already provided.
    let has_expiration_tag = tags.iter().chain(extra_tags.iter()).any(|t| {
        matches!(t.kind(), TagKind::Expiration)
            || (matches!(t.kind(), TagKind::Custom(ref c) if c == "expiration"))
    });
    if !has_expiration_tag {
        if let Some(expiration_timestamp) = get_expiration_timestamp_for_kind(kind) {
            tags.push(Tag::expiration(Timestamp::from(
                expiration_timestamp as u64,
            )));
        }
    }

    tags.extend(extra_tags);
    let tags = Tags::from_list(tags);

    EventBuilder::new(nostr::Kind::Custom(kind), content)
        .tags(tags)
        .sign_with_keys(keys)
}

/// Creates a new order event (kind 38383)
///
/// # Arguments
///
/// * `keys` - The keys used to sign the event
/// * `content` - The content of the event
/// * `identifier` - The nip33 d tag (order ID) used to replace the event
/// * `extra_tags` - Additional tags for the event
///
/// # Returns
/// Returns a new order event
pub fn new_order_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
) -> Result<Event, Error> {
    create_event(
        keys,
        content,
        identifier,
        extra_tags,
        NOSTR_ORDER_EVENT_KIND,
    )
}

/// Creates a new rating event (kind 38384)
///
/// # Arguments
///
/// * `keys` - The keys used to sign the event
/// * `content` - The content of the event
/// * `identifier` - The nip33 d tag (user pubkey) used to replace the event
/// * `extra_tags` - Additional tags for the event
///
/// # Returns
/// Returns a new rating event
pub fn new_rating_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
) -> Result<Event, Error> {
    create_event(
        keys,
        content,
        identifier,
        extra_tags,
        NOSTR_RATING_EVENT_KIND,
    )
}

/// Creates a new info event (kind 38385)
///
/// # Arguments
///
/// * `keys` - The keys used to sign the event
/// * `content` - The content of the event
/// * `identifier` - The nip33 d tag (mostro pubkey) used to replace the event
/// * `extra_tags` - Additional tags for the event
///
/// # Returns
/// Returns a new info event
pub fn new_info_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
) -> Result<Event, Error> {
    create_event(keys, content, identifier, extra_tags, NOSTR_INFO_EVENT_KIND)
}

/// Creates a new dispute event (kind 38386)
///
/// # Arguments
///
/// * `keys` - The keys used to sign the event
/// * `content` - The content of the event
/// * `identifier` - The nip33 d tag (dispute ID) used to replace the event
/// * `extra_tags` - Additional tags for the event
///
/// # Returns
/// Returns a new dispute event
pub fn new_dispute_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
) -> Result<Event, Error> {
    create_event(
        keys,
        content,
        identifier,
        extra_tags,
        NOSTR_DISPUTE_EVENT_KIND,
    )
}

/// Creates a new exchange rates event (kind 30078, NIP-33)
///
/// This event publishes Bitcoin/fiat exchange rates to Nostr relays,
/// enabling censorship-resistant rate fetching for mobile clients.
///
/// # Arguments
///
/// * `keys` - The keys used to sign the event (Mostro's keypair)
/// * `content` - JSON-encoded exchange rates in Yadio format (e.g., `{"BTC": {"USD": 50000.0, ...}}`)
/// * `extra_tags` - Additional tags for the event (e.g., `updated_at`, `source`)
///
/// # Returns
/// Returns a new exchange rates event or an error
///
/// # Example
///
/// ```ignore
/// use std::collections::HashMap;
/// // Wrap rates in Yadio format: {"BTC": {"USD": 50000.0, ...}}
/// let mut wrapper = HashMap::new();
/// wrapper.insert("BTC".to_string(), bitcoin_prices.clone());
/// let content = serde_json::to_string(&wrapper)?;
/// let tags = Tags::from_list(vec![
///     Tag::custom(TagKind::Custom("published_at".into()), vec![timestamp.to_string()]),
///     Tag::custom(TagKind::Custom("source".into()), vec!["yadio".to_string()]),
///     Tag::expiration(Timestamp::from(expiration)),
/// ]);
/// let event = new_exchange_rates_event(&keys, &content, tags)?;
/// ```
pub fn new_exchange_rates_event(
    keys: &Keys,
    content: &str,
    extra_tags: Tags,
) -> Result<Event, Error> {
    create_event(
        keys,
        content,
        "mostro-rates".to_string(), // NIP-33 d tag identifier
        extra_tags,
        NOSTR_EXCHANGE_RATES_EVENT_KIND,
    )
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
            (now.as_secs() - data.2 as u64) / SECONDS_IN_DAY
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
    // `WaitingTakerBond` is the daemon-internal "matched, awaiting bond"
    // state (Phase 1.5). On the wire it publishes as `pending` (per
    // `create_status_tags`), so range-order min/max advertising must
    // mirror the `Pending` branch — otherwise the bond window would
    // expose a single `fiat_amount` and clients would think the order
    // had moved out of the range-takeable state.
    if order.status == Status::Pending.to_string()
        || order.status == Status::WaitingTakerBond.to_string()
    {
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

pub(crate) fn create_platform_tag_values(instance_name: Option<&str>) -> Vec<String> {
    std::iter::once("mostro")
        .chain(instance_name.map(str::trim).filter(|s| !s.is_empty()))
        .map(String::from)
        .collect()
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
        // Phase 1.5: an order with a prospective taker mid-bond is
        // daemon-internally `WaitingTakerBond`, but on the wire it must
        // still publish as `pending` so it stays advertised under
        // NIP-69's four-bucket model (`docs/ANTI_ABUSE_BOND.md` §2
        // principle 8). A malicious taker who never pays cannot park
        // the order off the book — concurrent takers race to lock.
        Status::WaitingTakerBond => Ok((true, Status::Pending)),
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
/// - Mostro daemon's pubkey (so clients can identify the instance)
///
/// The resulting reference uses a custom format:
/// `mostro:{order_id}?relays={relay1,relay2,...}&mostro={pubkey}`
///
fn create_source_tag(
    order: &Order,
    mostro_relays: &[String],
    mostro_pubkey: &str,
) -> Result<Option<String>, MostroError> {
    // Source tag is also emitted while the order is in `WaitingTakerBond`
    // (Phase 1.5). The wire-published status maps to `pending`, so
    // clients discovering the order must still be able to construct
    // the reference URL.
    if order.status == Status::Pending.to_string()
        || order.status == Status::WaitingTakerBond.to_string()
    {
        // Create a mostro: custom source reference for pending orders
        // Include the Mostro pubkey so clients can identify the instance
        let custom_ref = format!(
            "mostro:{}?relays={}&mostro={}",
            order.id,
            mostro_relays.join(","),
            mostro_pubkey
        );

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
/// - `y`: "mostro" platform identifier, plus optional Mostro instance name from settings
/// - `z`: Always "order" (event type)
/// - `rating`: User reputation data (if available)
/// - `source`: mostro: scheme link to pending orders (`mostro:{order_id}?relays={...}&mostro={pubkey}`)
///
/// # Arguments
///
/// * `order` - The order to transform into tags
/// * `reputation_data` - Optional reputation data for the maker
/// * `mostro_pubkey` - Optional Mostro pubkey override. If None, derived from get_keys().
///   Pass Some() in tests to avoid global state dependencies.
///
pub fn order_to_tags(
    order: &Order,
    reputation_data: Option<(f64, i64, i64)>,
    mostro_pubkey: Option<&str>,
) -> Result<Option<Tags>, MostroError> {
    // Position of the tags in the list
    const RATING_TAG_INDEX: usize = 7;
    const SOURCE_TAG_INDEX: usize = 8;

    // Check if the order is pending/in-progress/success/canceled
    let (create_event, status) = create_status_tags(order)?;
    // Create mostro: scheme link in case of pending order creation
    // Include the Mostro pubkey so clients can identify the instance
    let pubkey = match mostro_pubkey {
        Some(pk) => pk.to_string(),
        None => get_keys()?.public_key().to_hex(),
    };
    let mostro_link = create_source_tag(order, &Settings::get_nostr().relays, &pubkey)?;

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
                TagKind::Custom(Cow::Borrowed("expires_at")),
                vec![order.expires_at.to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("expiration")),
                vec![get_expiration_timestamp_for_kind(NOSTR_ORDER_EVENT_KIND)
                    .expect("expiration is always defined for order events")
                    .to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("y")),
                create_platform_tag_values(Settings::get_mostro().name.as_deref()),
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
    let bond_settings = Settings::get_bond();
    // DEPRECATED(v0.19.0, #786): once the `transport` knob is gone the
    // `protocol_version` tag is hardcoded to the crate-wide `PROTOCOL_VER`.
    #[allow(deprecated)]
    let protocol_version = mostro_settings.transport.protocol_version();

    let mut tags_vec: Vec<Tag> = vec![
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
        // Capability advertisement: which Mostro protocol version this node
        // speaks ("1" = gift wrap, "2" = NIP-44 direct), derived from the
        // `transport` setting so clients pick the right wire format before
        // sending anything. See docs/TRANSPORT_V2_SPEC.md.
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("protocol_version")),
            vec![protocol_version.to_string()],
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
            create_platform_tag_values(mostro_settings.name.as_deref()),
        ),
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("z")),
            vec!["info".to_string()],
        ),
    ];

    tags_vec.extend(bond_policy_tags(bond_settings));

    Tags::from_list(tags_vec)
}

/// Build the bond policy tag block for the info event.
///
/// `bond_enabled` is always emitted so clients can disambiguate "bond
/// feature off on this node" from "older daemon that doesn't speak bond
/// at all". The remaining tags are present only when the feature is
/// enabled — together they let a client warn the user about bond cost,
/// scope, and slash policy *before* the take/create flow starts, and
/// render any deadline (slashed_at + payout_claim_window_days) in the
/// user's own locale without Mostro shipping any hardcoded text.
///
/// Split out from [`info_to_tags`] so unit tests can exercise both the
/// disabled and enabled branches without mutating the `MOSTRO_CONFIG`
/// OnceLock that the parent function reads from.
fn bond_policy_tags(
    bond_settings: Option<&crate::config::types::AntiAbuseBondSettings>,
) -> Vec<Tag> {
    let mut tags = Vec::with_capacity(7);
    let bond_enabled = bond_settings.is_some_and(|b| b.enabled);
    tags.push(Tag::custom(
        TagKind::Custom(Cow::Borrowed("bond_enabled")),
        vec![bond_enabled.to_string()],
    ));
    if let Some(bond) = bond_settings {
        if bond.enabled {
            let apply_to_str = match bond.apply_to {
                BondApplyTo::Take => "take",
                BondApplyTo::Make => "make",
                BondApplyTo::Both => "both",
            };
            tags.push(Tag::custom(
                TagKind::Custom(Cow::Borrowed("bond_amount_pct")),
                vec![bond.amount_pct.to_string()],
            ));
            tags.push(Tag::custom(
                TagKind::Custom(Cow::Borrowed("bond_base_amount_sats")),
                vec![bond.base_amount_sats.to_string()],
            ));
            tags.push(Tag::custom(
                TagKind::Custom(Cow::Borrowed("bond_apply_to")),
                vec![apply_to_str.to_string()],
            ));
            tags.push(Tag::custom(
                TagKind::Custom(Cow::Borrowed("bond_slash_on_waiting_timeout")),
                vec![bond.slash_on_waiting_timeout.to_string()],
            ));
            tags.push(Tag::custom(
                TagKind::Custom(Cow::Borrowed("bond_slash_node_share_pct")),
                vec![bond.slash_node_share_pct.to_string()],
            ));
            tags.push(Tag::custom(
                TagKind::Custom(Cow::Borrowed("bond_payout_claim_window_days")),
                vec![bond.payout_claim_window_days.to_string()],
            ));
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::create_platform_tag_values;
    use super::create_status_tags;
    use super::{info_to_tags, order_to_tags};
    use crate::app::context::test_utils::test_settings;
    use crate::config::MOSTRO_CONFIG;
    use crate::lightning::LnStatus;
    use mostro_core::prelude::*;
    use nostr_sdk::prelude::*;
    use std::borrow::Cow;

    // ── Shared test helpers ──────────────────────────────────────────────────────

    /// Test Mostro pubkey (derived from the test nsec in test_settings)
    const TEST_MOSTRO_PUBKEY: &str =
        "9a0e40e008c6dcfdb3c608a65ddf1c4e72eed7eeefbe1eb88ea0f1ea8b43dc4d";

    /// Initialize global settings once per test binary run using the canonical
    /// test_settings() helper from AppContext test_utils — consistent with the
    /// rest of the test infrastructure.
    /// Uses `let _ =` to silently ignore if the OnceLock is already set by another test.
    fn init_test_settings() {
        let _ = MOSTRO_CONFIG.set(test_settings());
    }

    /// Build a minimal pending order sufficient for order_to_tags to emit tags.
    fn make_pending_order() -> Order {
        Order {
            status: Status::Pending.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            payment_method: "bank".to_string(),
            ..Default::default()
        }
    }

    /// Build a stub LnStatus sufficient for info_to_tags.
    fn make_ln_status() -> LnStatus {
        LnStatus {
            version: "0.0.0".to_string(),
            node_pubkey: "stub".to_string(),
            commit_hash: "stub".to_string(),
            node_alias: "stub".to_string(),
            chains: vec![],
            networks: vec![],
            uris: vec![],
        }
    }

    /// Extract the values of the "y" tag from a Tags collection.
    ///
    /// `tag.as_vec()` returns `[tag_name, val0, val1, ...]`, so values start at index 1.
    /// Returns None if no "y" tag is present — which itself would be a test failure.
    fn get_y_tag_values(tags: &Tags) -> Option<Vec<String>> {
        tags.iter().find_map(|tag| {
            let vec = tag.clone().to_vec();
            if vec.first().map(|s| s.as_str()) == Some("y") {
                Some(vec[1..].to_vec())
            } else {
                None
            }
        })
    }

    // ── create_platform_tag_values unit tests (unchanged from #653) ──────────────

    #[test]
    fn create_platform_tag_values_with_none_returns_only_mostro() {
        assert_eq!(create_platform_tag_values(None), vec!["mostro".to_string()]);
    }

    #[test]
    fn create_platform_tag_values_with_name_appends_trimmed_name() {
        assert_eq!(
            create_platform_tag_values(Some("  name  ")),
            vec!["mostro".to_string(), "name".to_string()]
        );
    }

    #[test]
    fn create_platform_tag_values_with_empty_string_returns_only_mostro() {
        assert_eq!(
            create_platform_tag_values(Some("")),
            vec!["mostro".to_string()]
        );
    }

    #[test]
    fn create_platform_tag_values_with_whitespace_only_returns_only_mostro() {
        assert_eq!(
            create_platform_tag_values(Some("   \t  ")),
            vec!["mostro".to_string()]
        );
    }

    // ── order_to_tags: end-to-end y-tag emission (kind 38383) ───────────────────

    #[test]
    fn order_to_tags_emits_y_tag_with_mostro_as_first_value() {
        init_test_settings();
        let order = make_pending_order();

        let tags = order_to_tags(&order, None, Some(TEST_MOSTRO_PUBKEY))
            .expect("order_to_tags must not error")
            .expect("pending order must produce Some(tags)");

        let y_values = get_y_tag_values(&tags).expect("order_to_tags must emit a y tag");

        assert_eq!(y_values[0], "mostro", "y[0] must always be 'mostro'");
    }

    #[test]
    fn order_to_tags_y_tag_matches_platform_helper_output() {
        init_test_settings();
        let order = make_pending_order();

        let tags = order_to_tags(&order, None, Some(TEST_MOSTRO_PUBKEY))
            .expect("order_to_tags must not error")
            .expect("pending order must produce Some(tags)");

        let y_values = get_y_tag_values(&tags).expect("order_to_tags must emit a y tag");

        let expected = create_platform_tag_values(test_settings().mostro.name.as_deref());
        assert_eq!(
            y_values, expected,
            "order_to_tags must wire create_platform_tag_values correctly into the y tag"
        );
    }

    // ── order_to_tags: source tag with Mostro pubkey (kind 38383) ───────────────

    /// Extract the value of the "source" tag from a Tags collection.
    fn get_source_tag_value(tags: &Tags) -> Option<String> {
        tags.iter().find_map(|tag| {
            let vec = tag.clone().to_vec();
            if vec.first().map(|s| s.as_str()) == Some("source") {
                vec.get(1).cloned()
            } else {
                None
            }
        })
    }

    #[test]
    fn order_to_tags_source_tag_includes_mostro_pubkey() {
        init_test_settings();
        let order = make_pending_order();

        let tags = order_to_tags(&order, None, Some(TEST_MOSTRO_PUBKEY))
            .expect("order_to_tags must not error")
            .expect("pending order must produce Some(tags)");

        let source = get_source_tag_value(&tags).expect("pending order must have source tag");

        // Verify the source tag format: mostro:{order_id}?relays={...}&mostro={pubkey}
        assert!(
            source.starts_with("mostro:"),
            "source must start with 'mostro:' scheme"
        );
        assert!(
            source.contains("&mostro="),
            "source must contain '&mostro=' parameter"
        );
        assert!(
            source.contains(&format!("&mostro={}", TEST_MOSTRO_PUBKEY)),
            "source must contain the correct Mostro pubkey"
        );
    }

    // ── info_to_tags: end-to-end y-tag emission (kind 38385) ────────────────────

    #[test]
    fn info_to_tags_emits_y_tag_with_mostro_as_first_value() {
        init_test_settings();
        let ln_status = make_ln_status();

        let tags = info_to_tags(&ln_status);

        let y_values = get_y_tag_values(&tags).expect("info_to_tags must emit a y tag");

        assert_eq!(y_values[0], "mostro", "y[0] must always be 'mostro'");
    }

    #[test]
    fn info_to_tags_y_tag_matches_platform_helper_output() {
        init_test_settings();
        let ln_status = make_ln_status();

        let tags = info_to_tags(&ln_status);

        let y_values = get_y_tag_values(&tags).expect("info_to_tags must emit a y tag");

        let expected = create_platform_tag_values(test_settings().mostro.name.as_deref());
        assert_eq!(
            y_values, expected,
            "info_to_tags must wire create_platform_tag_values correctly into the y tag"
        );
    }

    /// Look up a single-value tag in a Tags collection, returning its
    /// first value. Helper for the bond-policy assertions below.
    fn get_tag_value(tags: &Tags, name: &str) -> Option<String> {
        tags.iter().find_map(|tag| {
            let vec = tag.clone().to_vec();
            if vec.first().map(String::as_str) == Some(name) {
                vec.get(1).cloned()
            } else {
                None
            }
        })
    }

    #[test]
    fn info_to_tags_emits_bond_disabled_marker_when_bond_off() {
        // test_settings() builds Settings with `anti_abuse_bond = None`,
        // i.e. the feature is off. `bond_enabled` must still be emitted
        // (as "false") so clients can disambiguate "feature off" from
        // "older daemon that doesn't speak bond at all". The rest of the
        // policy tags must be absent.
        init_test_settings();
        let ln_status = make_ln_status();

        let tags = info_to_tags(&ln_status);

        assert_eq!(
            get_tag_value(&tags, "bond_enabled").as_deref(),
            Some("false"),
            "bond_enabled must be emitted as 'false' when the feature is off"
        );

        for absent in [
            "bond_amount_pct",
            "bond_base_amount_sats",
            "bond_apply_to",
            "bond_slash_on_waiting_timeout",
            "bond_slash_node_share_pct",
            "bond_payout_claim_window_days",
        ] {
            assert!(
                get_tag_value(&tags, absent).is_none(),
                "{absent} must be absent when the bond feature is disabled"
            );
        }
    }

    /// Build a `Tags` collection from a bond settings snapshot via the
    /// pure `bond_policy_tags` helper. Exists because
    /// `info_to_tags` itself reads bond settings from the
    /// `MOSTRO_CONFIG` OnceLock — which is shared across the test
    /// binary and cannot be mutated mid-run — so we exercise the
    /// enabled branch through the helper directly.
    fn bond_tags(bond: Option<&crate::config::types::AntiAbuseBondSettings>) -> Tags {
        Tags::from_list(super::bond_policy_tags(bond))
    }

    #[test]
    fn info_to_tags_emits_bond_enabled_marker_when_bond_on() {
        // Companion of `info_to_tags_emits_bond_disabled_marker_when_bond_off`.
        // Verifies every advertised policy tag is present and that the
        // emitted value mirrors the source settings byte-for-byte —
        // clients parse these as text, so any reformat by `to_string`
        // would silently break them.
        let bond = crate::config::types::AntiAbuseBondSettings {
            enabled: true,
            amount_pct: 0.02,
            base_amount_sats: 2_500,
            apply_to: crate::config::types::BondApplyTo::Both,
            slash_on_waiting_timeout: true,
            slash_node_share_pct: 0.4,
            payout_invoice_window_seconds: 300,
            payout_max_retries: 5,
            payout_claim_window_days: 30,
        };

        let tags = bond_tags(Some(&bond));

        assert_eq!(
            get_tag_value(&tags, "bond_enabled").as_deref(),
            Some("true"),
            "bond_enabled must be emitted as 'true' when the feature is on"
        );
        assert_eq!(
            get_tag_value(&tags, "bond_amount_pct").as_deref(),
            Some("0.02")
        );
        assert_eq!(
            get_tag_value(&tags, "bond_base_amount_sats").as_deref(),
            Some("2500")
        );
        assert_eq!(
            get_tag_value(&tags, "bond_apply_to").as_deref(),
            Some("both")
        );
        assert_eq!(
            get_tag_value(&tags, "bond_slash_on_waiting_timeout").as_deref(),
            Some("true")
        );
        assert_eq!(
            get_tag_value(&tags, "bond_slash_node_share_pct").as_deref(),
            Some("0.4")
        );
        assert_eq!(
            get_tag_value(&tags, "bond_payout_claim_window_days").as_deref(),
            Some("30")
        );
    }

    // ── Dispute event tag list: end-to-end y-tag emission (kind 38386) ──────────

    /// Verifies that the tag list built for dispute events emits the correct y tag.
    ///
    /// Mirrors the exact inline tag construction used in `publish_dispute_event` and
    /// `close_dispute_after_user_resolution` in src/app/dispute.rs, as well as the
    /// admin handlers in admin_cancel.rs, admin_settle.rs, and admin_take_dispute.rs.
    /// All five callsites use the identical pattern verified here.
    #[test]
    fn dispute_event_tags_emit_y_tag_matching_platform_helper() {
        init_test_settings();

        let tags = Tags::from_list(vec![
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("s")),
                vec!["initiated-by-buyer".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("initiator")),
                vec!["buyer".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("y")),
                create_platform_tag_values(test_settings().mostro.name.as_deref()),
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("z")),
                vec!["dispute".to_string()],
            ),
        ]);

        let y_values = get_y_tag_values(&tags)
            .expect("y tag must be present in dispute event tags (kind 38386)");

        let expected = create_platform_tag_values(test_settings().mostro.name.as_deref());

        assert_eq!(y_values[0], "mostro", "y[0] must always be 'mostro'");
        assert_eq!(
            y_values, expected,
            "dispute event tag list must wire create_platform_tag_values correctly"
        );
    }

    // ── Dev-fee audit event tag list: end-to-end y-tag emission (kind 8383) ─────

    /// Verifies that the tag list built for dev-fee audit events emits the correct y tag.
    ///
    /// Mirrors the exact inline tag construction in `publish_dev_fee_audit_event`
    /// in src/util.rs (line ~602). This is a regression guard: if the y-tag call is
    /// accidentally removed from that function, this test will catch it.
    #[test]
    fn dev_fee_audit_event_tags_emit_y_tag_matching_platform_helper() {
        init_test_settings();

        let tags = Tags::from_list(vec![
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("order-id")),
                vec!["00000000-0000-0000-0000-000000000000".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("amount")),
                vec!["300".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("hash")),
                vec!["deadbeef".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("destination")),
                vec!["dev@lightning.address".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("network")),
                vec!["mainnet".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("y")),
                create_platform_tag_values(test_settings().mostro.name.as_deref()),
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("z")),
                vec!["dev-fee-payment".to_string()],
            ),
        ]);

        let y_values = get_y_tag_values(&tags)
            .expect("y tag must be present in dev-fee audit event tags (kind 8383)");

        let expected = create_platform_tag_values(test_settings().mostro.name.as_deref());

        assert_eq!(y_values[0], "mostro", "y[0] must always be 'mostro'");
        assert_eq!(
            y_values, expected,
            "dev-fee audit event tag list must wire create_platform_tag_values correctly"
        );
    }

    // ── Phase 1.5 NIP-69 mapping tests ───────────────────────────────────

    /// Load-bearing for the non-blockability invariant
    /// (`docs/ANTI_ABUSE_BOND.md` §2 principle 8): an order whose
    /// daemon-internal status is `WaitingTakerBond` must publish on
    /// the wire with status `Pending`, identical to a no-taker order,
    /// so it stays advertised in NIP-69's `pending` bucket and other
    /// takers can race for it.
    #[test]
    fn waiting_taker_bond_maps_to_pending_on_wire() {
        let mut order = make_pending_order();
        order.status = Status::WaitingTakerBond.to_string();

        let (emit, mapped) = create_status_tags(&order).expect("status tags");
        assert!(
            emit,
            "WaitingTakerBond must emit the order event so the orderbook keeps showing it"
        );
        assert_eq!(
            mapped,
            Status::Pending,
            "WaitingTakerBond must publish as Pending on the wire (NIP-69 invariant)"
        );
    }

    /// Phase 5 (`docs/ANTI_ABUSE_BOND.md` §10.1 / §10.4): an order whose
    /// daemon-internal status is `WaitingMakerBond` has **not** been
    /// published to Nostr yet — the maker's bond is still outstanding.
    /// `create_status_tags` must therefore signal "do not emit an event"
    /// (`create_event == false`), so the order never appears in the book
    /// until the bond locks and the order transitions to `Pending`. This
    /// is the opposite of `WaitingTakerBond`, which is already advertised
    /// and must keep emitting.
    #[test]
    fn waiting_maker_bond_is_not_published_on_wire() {
        let mut order = make_pending_order();
        order.status = Status::WaitingMakerBond.to_string();

        let (emit, _mapped) = create_status_tags(&order).expect("status tags");
        assert!(
            !emit,
            "WaitingMakerBond must NOT emit an order event — the order is invisible until the bond locks"
        );
    }

    /// Sanity: the existing `Pending` mapping behaves identically. If
    /// somebody refactors `create_status_tags` the bucket-equivalence
    /// between `Pending` and `WaitingTakerBond` must not drift.
    #[test]
    fn pending_and_waiting_taker_bond_publish_the_same_wire_status() {
        let mut pending = make_pending_order();
        pending.status = Status::Pending.to_string();
        let mut wtb = make_pending_order();
        wtb.status = Status::WaitingTakerBond.to_string();

        let (emit_p, status_p) = create_status_tags(&pending).expect("status tags pending");
        let (emit_w, status_w) = create_status_tags(&wtb).expect("status tags wtb");

        assert_eq!(emit_p, emit_w);
        assert_eq!(status_p, status_w);
    }
}
