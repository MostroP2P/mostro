// File with the types for the configuration settings
// Initialize the types for the configuration settings
use crate::config::constants::DEV_FEE_AUDIT_EVENT_KIND;
use crate::config::MOSTRO_CONFIG;
use mostro_core::prelude::*;
use serde::{Deserialize, Serialize};

/// Scope of the anti-abuse bond enforcement.
///
/// `apply_to` in `[anti_abuse_bond]` selects which trade flow(s) must post a
/// bond. The feature is deliberately scoped so operators can roll it out one
/// side at a time (taker first, maker later) as successive phases land.
#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BondApplyTo {
    #[default]
    Take,
    Make,
    Both,
}

impl BondApplyTo {
    /// True when the taker side of a trade must lock a bond.
    pub fn applies_to_taker(self) -> bool {
        matches!(self, BondApplyTo::Take | BondApplyTo::Both)
    }

    /// True when the maker side of a trade must lock a bond.
    pub fn applies_to_maker(self) -> bool {
        matches!(self, BondApplyTo::Make | BondApplyTo::Both)
    }
}

/// Anti-abuse bond configuration (issue #711).
///
/// Opt-in. When `enabled = false` (the default) every code path added by
/// this feature remains inert — existing orders behave exactly as before.
/// See `docs/ANTI_ABUSE_BOND.md` for the full phased rollout.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AntiAbuseBondSettings {
    /// Master switch. When false, no bond is required and no slashing occurs.
    #[serde(default)]
    pub enabled: bool,
    /// Fraction of the order amount used for the bond (0.01 = 1%). The
    /// actual bond is `max(amount_pct * order_amount_sats, base_amount_sats)`.
    ///
    /// Named `amount_pct` (not `amount_sats`) because the value is a
    /// unitless fraction, not a sat quantity — the latter would conflict
    /// with `Bond::amount_sats`, which *is* an integer sat amount.
    #[serde(default = "default_bond_amount_pct")]
    pub amount_pct: f64,
    /// Floor applied to the bond computation, in satoshis.
    #[serde(default = "default_bond_base_amount")]
    pub base_amount_sats: i64,
    /// Which trade flow(s) require the bond.
    #[serde(default)]
    pub apply_to: BondApplyTo,
    /// Slash the bond when the bonded party lets the waiting-state timeout
    /// actually elapse. A cancellation before the timeout MUST always
    /// release the bond regardless of this flag; see §Phase 4 of the spec.
    ///
    /// Note: there is intentionally no `slash_on_lost_dispute` flag.
    /// Dispute slashes are expressed by the solver per-resolution via the
    /// `BondResolution` payload (see §3 / Phase 2 of the spec).
    #[serde(default)]
    pub slash_on_waiting_timeout: bool,
    /// Fraction of a slashed bond that the node retains. The remainder is
    /// paid out to the winning counterparty. The node share is meant to
    /// fund solver compensation for dispute work; see §15.4 of the spec.
    /// `0.0` = full payout to counterparty (legacy behaviour);
    /// `1.0` = node keeps everything. Used by Phase 3.
    ///
    /// Validated at deserialization: rejected if outside `[0.0, 1.0]`.
    /// Out-of-range values would corrupt the
    /// `node_share_sats = floor(amount_sats * pct)` math (negative
    /// counterparty share, or node retention exceeding the bond), so the
    /// daemon refuses to start rather than silently misbehave.
    #[serde(
        default = "default_slash_node_share_pct",
        deserialize_with = "deserialize_slash_node_share_pct"
    )]
    pub slash_node_share_pct: f64,
    /// How long (seconds) Mostro waits between payout-invoice retries
    /// when asking the winning counterparty for a bolt11. Used by Phase 3.
    #[serde(default = "default_payout_invoice_window_seconds")]
    pub payout_invoice_window_seconds: u64,
    /// Maximum number of `send_payment` retries against an invoice the
    /// counterparty has already submitted before a bond transitions to
    /// `failed`. Independent from how long we wait for the invoice itself
    /// (that is governed by `payout_claim_window_days`). Used by Phase 3.
    #[serde(default = "default_payout_max_retries")]
    pub payout_max_retries: u32,
    /// How many days the winning counterparty has, from the moment the
    /// bond is slashed, to claim their share by submitting a payout
    /// bolt11. If the window elapses without an invoice ever being
    /// received, the bond transitions to `forfeited` and the node retains
    /// the counterparty share too (long-stop forfeiture; see §15.4).
    /// Used by Phase 3.
    #[serde(default = "default_payout_claim_window_days")]
    pub payout_claim_window_days: u32,
}

fn default_bond_amount_pct() -> f64 {
    0.01
}

fn default_bond_base_amount() -> i64 {
    1_000
}

fn default_payout_invoice_window_seconds() -> u64 {
    300
}

fn default_payout_max_retries() -> u32 {
    5
}

fn default_slash_node_share_pct() -> f64 {
    0.5
}

/// Validating deserializer for `slash_node_share_pct`. Rejects anything
/// outside `[0.0, 1.0]` (including NaN) with a descriptive serde error
/// so a typo in the operator's `settings.toml` fails fast at startup
/// rather than producing nonsense math at slash time.
fn deserialize_slash_node_share_pct<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let v = f64::deserialize(deserializer)?;
    if !(0.0..=1.0).contains(&v) {
        return Err(D::Error::custom(format!(
            "slash_node_share_pct must be in [0.0, 1.0], got {v}"
        )));
    }
    Ok(v)
}

fn default_payout_claim_window_days() -> u32 {
    15
}

impl Default for AntiAbuseBondSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            amount_pct: default_bond_amount_pct(),
            base_amount_sats: default_bond_base_amount(),
            apply_to: BondApplyTo::default(),
            slash_on_waiting_timeout: false,
            slash_node_share_pct: default_slash_node_share_pct(),
            payout_invoice_window_seconds: default_payout_invoice_window_seconds(),
            payout_max_retries: default_payout_max_retries(),
            payout_claim_window_days: default_payout_claim_window_days(),
        }
    }
}

/// Selects the escrow backend for this node. Resolved from `[cashu].enabled`
/// at startup; defaults to `Lightning` when the `[cashu]` block is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EscrowMode {
    #[default]
    Lightning,
    Cashu,
}

/// Cashu 2-of-3 multisig escrow configuration.
///
/// Opt-in. When `enabled = false` (the default) every code path added by this
/// feature remains inert — existing orders behave exactly as before. Mutually
/// exclusive with `[anti_abuse_bond]`; the daemon refuses to start if both are
/// enabled. See `docs/CASHU_ESCROW_ARCHITECTURE.md` for the full spec.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct CashuSettings {
    /// Master switch. When false, Lightning escrow is used.
    #[serde(default)]
    pub enabled: bool,
    /// URL of the Cashu mint all trades on this node will use.
    #[serde(default)]
    pub mint_url: String,
}

/// Event expiration configuration settings
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct ExpirationSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_days: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_days: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispute_days: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_audit_days: Option<u32>,
}

impl ExpirationSettings {
    /// Get expiration days for a specific event kind
    pub fn get_expiration_for_kind(&self, kind: u16) -> Option<u32> {
        match kind {
            NOSTR_ORDER_EVENT_KIND => self.order_days.or(Some(30)), // orders
            NOSTR_RATING_EVENT_KIND => self.rating_days.or(Some(90)), // ratings
            NOSTR_DISPUTE_EVENT_KIND => self.dispute_days.or(Some(90)), // disputes
            DEV_FEE_AUDIT_EVENT_KIND => self.fee_audit_days.or(Some(365)), // fee audits
            _ => None, // unknown kinds don't get expiration
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_kind_uses_configured_days() {
        let settings = ExpirationSettings {
            rating_days: Some(30),
            ..Default::default()
        };
        assert_eq!(
            settings.get_expiration_for_kind(NOSTR_RATING_EVENT_KIND),
            Some(30)
        );
    }

    #[test]
    fn rating_kind_falls_back_to_90_when_unconfigured() {
        let settings = ExpirationSettings::default();
        assert_eq!(
            settings.get_expiration_for_kind(NOSTR_RATING_EVENT_KIND),
            Some(90)
        );
    }

    #[test]
    fn order_kind_falls_back_to_30_when_unconfigured() {
        let settings = ExpirationSettings::default();
        assert_eq!(
            settings.get_expiration_for_kind(NOSTR_ORDER_EVENT_KIND),
            Some(30)
        );
    }

    #[test]
    fn dispute_kind_falls_back_to_90_when_unconfigured() {
        let settings = ExpirationSettings::default();
        assert_eq!(
            settings.get_expiration_for_kind(NOSTR_DISPUTE_EVENT_KIND),
            Some(90)
        );
    }

    #[test]
    fn unknown_kind_returns_none() {
        let settings = ExpirationSettings::default();
        assert_eq!(settings.get_expiration_for_kind(12345), None);
    }
}

// / Implement the TryFrom trait for each of the structs in Settings
// / This allows you to convert from Settings to each of the structs directly.
macro_rules! impl_try_from_settings {
    ($($ty:ty => $field:ident),*) => {
        $(
            impl TryFrom<super::Settings> for $ty {
                type Error = mostro_core::error::MostroError;

                fn try_from(_: super::Settings) -> Result<Self, Self::Error> {
                    Ok(MOSTRO_CONFIG.get().unwrap().$field.clone())
                }
            }
        )*
    };
}
/// Database configuration settings
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct DatabaseSettings {
    /// Database connection URL (e.g., "postgres://user:pass@localhost/dbname")  
    pub url: String,
}
/// Lightning configuration settings
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct LightningSettings {
    /// LND certificate file path
    pub lnd_cert_file: String,
    /// LND macaroon file path
    pub lnd_macaroon_file: String,
    /// LND gRPC host
    pub lnd_grpc_host: String,
    /// Invoice expiration window in seconds
    pub invoice_expiration_window: u32,
    /// Hold invoice CLTV delta
    pub hold_invoice_cltv_delta: u32,
    /// Hold invoice expiration window in seconds
    pub hold_invoice_expiration_window: u32,
    /// Number of payment attempts
    pub payment_attempts: u32,
    /// Payment retries interval in seconds
    pub payment_retries_interval: u32,
}
/// Nostr configuration settings
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct NostrSettings {
    /// Nostr private key. Optional when `MOSTRO_NSEC_PRIVKEY` is provided via
    /// environment variable or `<settings_dir>/.env`.
    #[serde(default)]
    pub nsec_privkey: String,
    /// Nostr relays list
    pub relays: Vec<String>,
}
/// RPC configuration settings
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RpcSettings {
    /// Enable RPC server
    pub enabled: bool,
    /// RPC server listen address
    pub listen_address: String,
    /// RPC server port
    pub port: u16,
    /// Duration in seconds after which inactive rate-limiter entries are evicted
    #[serde(default = "default_rate_limiter_stale_duration")]
    pub rate_limiter_stale_duration: u64,
}

fn default_rate_limiter_stale_duration() -> u64 {
    3600
}

impl Default for RpcSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_address: "127.0.0.1".to_string(),
            port: 50051,
            rate_limiter_stale_duration: default_rate_limiter_stale_duration(),
        }
    }
}

/// Mostro configuration settings

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MostroSettings {
    /// Fee percentage for the Mostro
    pub fee: f64,
    /// Maximum routing fee percentage
    pub max_routing_fee: f64,
    /// Maximum order amount
    pub max_order_amount: u32,
    /// Minimum payment amount
    pub min_payment_amount: u32,
    /// Expiration hours
    pub expiration_hours: u32,
    /// Expiration seconds
    pub expiration_seconds: u32,
    /// User rates sent interval seconds
    pub user_rates_sent_interval_seconds: u32,
    /// Maximum expiration days
    pub max_expiration_days: u32,
    /// Publish relays interval
    pub publish_relays_interval: u32,
    /// Proof of work required
    pub pow: u8,
    /// Publish mostro info interval
    pub publish_mostro_info_interval: u32,
    /// Bitcoin price API base URL
    pub bitcoin_price_api_url: String,
    /// Fiat currencies accepted for orders (empty list accepts all)
    pub fiat_currencies_accepted: Vec<String>,
    /// Maximum orders per response in orders action
    pub max_orders_per_response: u8,
    /// Development fee as percentage of Mostro fee (0.10 to 1.0)
    /// Example: 0.30 means 30% of the Mostro fee goes to development
    pub dev_fee_percentage: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// Publish exchange rates to Nostr (kind 30078, NIP-33)
    #[serde(default = "default_publish_exchange_rates")]
    pub publish_exchange_rates_to_nostr: bool,
    /// Exchange rates update interval in seconds (default: 300 = 5 minutes)
    #[serde(default = "default_exchange_rates_update_interval")]
    pub exchange_rates_update_interval_seconds: u64,
}

fn default_publish_exchange_rates() -> bool {
    true // Enable by default for censorship resistance
}

fn default_exchange_rates_update_interval() -> u64 {
    300 // 5 minutes
}

impl Default for MostroSettings {
    fn default() -> Self {
        Self {
            fee: 0.0,
            max_routing_fee: 0.002,
            max_order_amount: 1000000,
            min_payment_amount: 100,
            expiration_hours: 24,
            expiration_seconds: 900,
            user_rates_sent_interval_seconds: 3600,
            max_expiration_days: 15,
            publish_relays_interval: 60,
            pow: 0,
            publish_mostro_info_interval: 300,
            bitcoin_price_api_url: "https://api.yadio.io".to_string(),
            fiat_currencies_accepted: vec![
                "USD".to_string(),
                "EUR".to_string(),
                "ARS".to_string(),
                "CUP".to_string(),
            ],
            max_orders_per_response: 10,
            dev_fee_percentage: 0.30,
            name: None,
            about: None,
            picture: None,
            website: None,
            publish_exchange_rates_to_nostr: default_publish_exchange_rates(),
            exchange_rates_update_interval_seconds: default_exchange_rates_update_interval(),
        }
    }
}

// Macro call here to implement the TryFrom trait for each of the structs in Settings
impl_try_from_settings!(
    DatabaseSettings => database,
    LightningSettings => lightning,
    NostrSettings => nostr,
    MostroSettings => mostro,
    RpcSettings => rpc
);

#[cfg(test)]
mod anti_abuse_bond_tests {
    use super::*;

    #[test]
    fn defaults_are_off() {
        let cfg = AntiAbuseBondSettings::default();
        assert!(!cfg.enabled);
        assert!(!cfg.slash_on_waiting_timeout);
        assert_eq!(cfg.apply_to, BondApplyTo::Take);
        assert_eq!(cfg.amount_pct, 0.01);
        assert_eq!(cfg.base_amount_sats, 1_000);
        assert_eq!(cfg.payout_invoice_window_seconds, 300);
        assert_eq!(cfg.payout_max_retries, 5);
        assert_eq!(cfg.slash_node_share_pct, 0.5);
        assert_eq!(cfg.payout_claim_window_days, 15);
    }

    #[test]
    fn apply_to_predicates() {
        assert!(BondApplyTo::Take.applies_to_taker());
        assert!(!BondApplyTo::Take.applies_to_maker());
        assert!(!BondApplyTo::Make.applies_to_taker());
        assert!(BondApplyTo::Make.applies_to_maker());
        assert!(BondApplyTo::Both.applies_to_taker());
        assert!(BondApplyTo::Both.applies_to_maker());
    }

    #[test]
    fn toml_omits_block() {
        #[derive(serde::Deserialize)]
        struct Stub {
            anti_abuse_bond: Option<AntiAbuseBondSettings>,
        }
        let parsed: Stub = toml::from_str("").expect("empty toml is valid");
        assert!(parsed.anti_abuse_bond.is_none());
    }

    #[test]
    fn toml_minimal_block_defaults() {
        #[derive(serde::Deserialize)]
        struct Stub {
            anti_abuse_bond: AntiAbuseBondSettings,
        }
        let parsed: Stub =
            toml::from_str("[anti_abuse_bond]\nenabled = true").expect("minimal block parses");
        assert!(parsed.anti_abuse_bond.enabled);
        // Unspecified fields fall back to the documented defaults.
        assert_eq!(parsed.anti_abuse_bond.apply_to, BondApplyTo::Take);
        assert_eq!(parsed.anti_abuse_bond.base_amount_sats, 1_000);
    }

    #[test]
    fn toml_apply_to_both() {
        #[derive(serde::Deserialize)]
        struct Stub {
            anti_abuse_bond: AntiAbuseBondSettings,
        }
        let parsed: Stub = toml::from_str(
            r#"[anti_abuse_bond]
enabled = true
apply_to = "both"
slash_on_waiting_timeout = true"#,
        )
        .expect("toml parses");
        assert_eq!(parsed.anti_abuse_bond.apply_to, BondApplyTo::Both);
        assert!(parsed.anti_abuse_bond.slash_on_waiting_timeout);
    }

    #[test]
    fn toml_slash_node_share_pct_and_claim_window_override() {
        #[derive(serde::Deserialize)]
        struct Stub {
            anti_abuse_bond: AntiAbuseBondSettings,
        }
        let parsed: Stub = toml::from_str(
            r#"[anti_abuse_bond]
enabled = true
slash_node_share_pct = 0.25
payout_claim_window_days = 30"#,
        )
        .expect("toml parses");
        assert_eq!(parsed.anti_abuse_bond.slash_node_share_pct, 0.25);
        assert_eq!(parsed.anti_abuse_bond.payout_claim_window_days, 30);
    }

    #[test]
    fn toml_slash_node_share_pct_boundaries_accepted() {
        #[derive(serde::Deserialize)]
        struct Stub {
            anti_abuse_bond: AntiAbuseBondSettings,
        }
        for (pct, expected) in [("0.0", 0.0), ("1.0", 1.0)] {
            let toml_str = format!("[anti_abuse_bond]\nslash_node_share_pct = {pct}");
            let parsed: Stub = toml::from_str(&toml_str).expect("boundary value should parse");
            assert_eq!(parsed.anti_abuse_bond.slash_node_share_pct, expected);
        }
    }

    #[test]
    fn toml_slash_node_share_pct_below_zero_rejected() {
        #[derive(Debug, serde::Deserialize)]
        struct Stub {
            #[allow(dead_code)]
            anti_abuse_bond: AntiAbuseBondSettings,
        }
        let err = toml::from_str::<Stub>("[anti_abuse_bond]\nslash_node_share_pct = -0.1")
            .expect_err("negative pct must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("slash_node_share_pct") && msg.contains("[0.0, 1.0]"),
            "error message should name the field and the valid range, got: {msg}"
        );
    }

    #[test]
    fn toml_slash_node_share_pct_above_one_rejected() {
        #[derive(Debug, serde::Deserialize)]
        struct Stub {
            #[allow(dead_code)]
            anti_abuse_bond: AntiAbuseBondSettings,
        }
        let err = toml::from_str::<Stub>("[anti_abuse_bond]\nslash_node_share_pct = 1.5")
            .expect_err("pct above 1.0 must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("slash_node_share_pct") && msg.contains("[0.0, 1.0]"),
            "error message should name the field and the valid range, got: {msg}"
        );
    }

    /// Backward compatibility: an operator who upgrades from a pre-spec-cleanup
    /// build may still have `slash_on_lost_dispute = true` in their
    /// `settings.toml`. The field has been removed from `AntiAbuseBondSettings`,
    /// but `deny_unknown_fields` is intentionally NOT set on the struct so the
    /// legacy line is silently ignored — no operator action required at upgrade
    /// time. This test locks that contract down so a future
    /// `#[serde(deny_unknown_fields)]` addition cannot accidentally break
    /// existing configs without an explicit migration.
    #[test]
    fn toml_legacy_slash_on_lost_dispute_parses() {
        #[derive(serde::Deserialize)]
        struct Stub {
            anti_abuse_bond: AntiAbuseBondSettings,
        }
        let parsed: Stub = toml::from_str(
            r#"[anti_abuse_bond]
enabled = true
slash_on_lost_dispute = true
slash_node_share_pct = 0.25
payout_claim_window_days = 30"#,
        )
        .expect("legacy slash_on_lost_dispute should be silently ignored");
        assert!(parsed.anti_abuse_bond.enabled);
        // Other fields on the same block must still deserialize correctly.
        assert_eq!(parsed.anti_abuse_bond.slash_node_share_pct, 0.25);
        assert_eq!(parsed.anti_abuse_bond.payout_claim_window_days, 30);
    }
}

#[cfg(test)]
mod cashu_settings_tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Stub {
        cashu: Option<CashuSettings>,
    }

    #[test]
    fn toml_cashu_absent_defaults_to_none() {
        let parsed: Stub = toml::from_str("").expect("empty toml is valid");
        assert!(parsed.cashu.is_none());
    }

    #[test]
    fn toml_cashu_disabled_by_default() {
        let parsed: Stub = toml::from_str("[cashu]\nmint_url = \"https://mint.example.com\"")
            .expect("minimal block parses");
        let cashu = parsed.cashu.expect("cashu block present");
        assert!(!cashu.enabled);
        assert_eq!(cashu.mint_url, "https://mint.example.com");
    }

    #[test]
    fn toml_cashu_minimal_block_enabled() {
        let parsed: Stub =
            toml::from_str("[cashu]\nenabled = true\nmint_url = \"https://mint.example.com\"")
                .expect("minimal enabled block parses");
        let cashu = parsed.cashu.expect("cashu block present");
        assert!(cashu.enabled);
        assert_eq!(cashu.mint_url, "https://mint.example.com");
    }
}
