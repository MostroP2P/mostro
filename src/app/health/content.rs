//! The encrypted `content` of the health event (protocol: `health_event.md`,
//! "Encrypted content"), version 1.
//!
//! Every field is serialized, `null` when unknown: `0` is a measurement,
//! `null` is "I don't know". `lnd` and `payouts` are `null` only on a Cashu
//! instance, `dev_fee` only when the instance pays no dev fee; `relays` and
//! `maintenance` are always objects.

use serde::Serialize;
use std::collections::BTreeMap;

/// Version of the payload schema.
pub const CONTENT_VERSION: u8 = 1;

/// Largest plaintext the protocol allows, in bytes.
pub const MAX_CONTENT_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HealthContent {
    pub v: u8,
    pub generated_at: i64,
    pub lnd: Option<LndContent>,
    pub payouts: Option<PayoutsContent>,
    pub dev_fee: Option<DevFeeContent>,
    pub relays: RelaysContent,
    pub maintenance: MaintenanceContent,
}

/// LND's state. When `reachable` is `false` every other field is `null`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LndContent {
    pub reachable: bool,
    pub synced_to_chain: Option<bool>,
    pub unsynced_since: Option<i64>,
    pub block_height: Option<u32>,
    pub best_header_timestamp: Option<i64>,
    pub peers: Option<u32>,
    pub channels_active: Option<u32>,
    pub channels_inactive: Option<u32>,
    pub channels_pending: Option<u32>,
    pub outbound_liquidity_sats: Option<i64>,
    pub inbound_liquidity_sats: Option<i64>,
    pub pending_htlcs: Option<u64>,
    pub nearest_htlc_expiry_blocks: Option<i64>,
}

impl LndContent {
    /// An LND that did not answer: `reachable = false`, everything else null.
    pub fn unreachable() -> Self {
        Self::default()
    }
}

/// What `ListChannels` adds to `GetInfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelFacts {
    pub outbound_liquidity_sats: i64,
    pub inbound_liquidity_sats: i64,
    pub pending_htlcs: u64,
    /// Minimum `expiration_height` over all pending HTLCs.
    pub nearest_htlc_expiration_height: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PayoutsContent {
    pub window_secs: i64,
    pub payouts_succeeded_1h: u64,
    pub payouts_failed_1h: u64,
    /// Distinct orders among `payouts_failed_1h`.
    pub payouts_failed_orders_1h: u64,
    pub failure_reasons_1h: BTreeMap<String, u64>,
    pub last_payout_success_at: Option<i64>,
    /// `null` only when the database could not be read (`payouts` check
    /// `unknown`, `database` check `fail`).
    pub settled_unpaid_orders: Option<u64>,
    /// Among `settled_unpaid_orders`, those settled over
    /// `PAYOUTS_STALE_SECS` ago, never attempted and with no payout in
    /// flight: nothing in the daemon will pay them (settled on a previous
    /// node, or left by an admin settle that failed before paying). They
    /// keep the `payouts` check at `warn` for the operator to clear them.
    /// `null` only when the database could not be read.
    pub stale_settled_unpaid_orders: Option<u64>,
    /// Age of the oldest settled unpaid order that is not stale.
    pub oldest_settled_unpaid_order_age_secs: Option<i64>,
    /// `null` only when the database could not be read.
    pub awaiting_buyer_invoice_orders: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DevFeeContent {
    /// `null` only when the database could not be read.
    pub pending: Option<u64>,
    pub failed_1h: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RelaysContent {
    pub configured: u64,
    pub connected: u64,
    pub last_outage_started_at: Option<i64>,
    pub last_outage_ended_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MaintenanceContent {
    pub enabled: bool,
    pub reason: Option<String>,
}

/// `nearest_htlc_expiry_blocks`: blocks from `block_height` to the nearest
/// HTLC expiry, zero or negative when it has passed, `None` without HTLCs.
pub fn nearest_htlc_expiry_blocks(
    nearest_expiration_height: Option<u32>,
    block_height: u32,
) -> Option<i64> {
    nearest_expiration_height.map(|height| i64::from(height) - i64::from(block_height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relays() -> RelaysContent {
        RelaysContent {
            configured: 4,
            connected: 4,
            last_outage_started_at: None,
            last_outage_ended_at: None,
        }
    }

    fn maintenance() -> MaintenanceContent {
        MaintenanceContent {
            enabled: false,
            reason: None,
        }
    }

    #[test]
    fn cashu_payload_has_null_parents_without_nested_fields() {
        let content = HealthContent {
            v: CONTENT_VERSION,
            generated_at: 1_792_000_000,
            lnd: None,
            payouts: None,
            dev_fee: None,
            relays: relays(),
            maintenance: maintenance(),
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["lnd"], serde_json::Value::Null);
        assert_eq!(json["payouts"], serde_json::Value::Null);
        assert_eq!(json["dev_fee"], serde_json::Value::Null);
        assert!(json["relays"].is_object());
        assert!(json["maintenance"].is_object());
        assert_eq!(json["v"], 1);
    }

    #[test]
    fn unreachable_lnd_is_an_object_with_null_fields() {
        let json = serde_json::to_value(LndContent::unreachable()).unwrap();
        let object = json.as_object().unwrap();
        assert_eq!(object["reachable"], false);
        assert_eq!(object.len(), 13, "every field is present");
        for (key, value) in object {
            if key != "reachable" {
                assert!(value.is_null(), "{key} must be null");
            }
        }
    }

    #[test]
    fn unknown_values_are_null_not_omitted() {
        let payouts = PayoutsContent {
            window_secs: 3_600,
            payouts_succeeded_1h: 0,
            payouts_failed_1h: 0,
            payouts_failed_orders_1h: 0,
            failure_reasons_1h: BTreeMap::new(),
            last_payout_success_at: None,
            settled_unpaid_orders: Some(0),
            stale_settled_unpaid_orders: Some(0),
            oldest_settled_unpaid_order_age_secs: None,
            awaiting_buyer_invoice_orders: Some(0),
        };
        let json = serde_json::to_value(&payouts).unwrap();
        assert_eq!(json["last_payout_success_at"], serde_json::Value::Null);
        assert_eq!(
            json["oldest_settled_unpaid_order_age_secs"],
            serde_json::Value::Null
        );
        assert_eq!(json["failure_reasons_1h"], serde_json::json!({}));
        assert_eq!(json["settled_unpaid_orders"], 0);
        let relays = serde_json::to_value(relays()).unwrap();
        assert_eq!(relays["last_outage_started_at"], serde_json::Value::Null);
        assert_eq!(relays["last_outage_ended_at"], serde_json::Value::Null);
    }

    #[test]
    fn htlc_expiry_is_relative_to_the_block_height() {
        assert_eq!(
            nearest_htlc_expiry_blocks(Some(971_640), 971_349),
            Some(291)
        );
        assert_eq!(nearest_htlc_expiry_blocks(Some(971_340), 971_349), Some(-9));
        assert_eq!(nearest_htlc_expiry_blocks(None, 971_349), None);
    }
}
