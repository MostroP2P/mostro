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
    Create,
    Both,
}

impl BondApplyTo {
    /// True when the taker side of a trade must lock a bond.
    pub fn applies_to_taker(self) -> bool {
        matches!(self, BondApplyTo::Take | BondApplyTo::Both)
    }

    /// True when the maker side of a trade must lock a bond.
    pub fn applies_to_maker(self) -> bool {
        matches!(self, BondApplyTo::Create | BondApplyTo::Both)
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
    /// Slash the bond when the bonded party loses a dispute.
    #[serde(default)]
    pub slash_on_lost_dispute: bool,
    /// Slash the bond when the bonded party lets the waiting-state timeout
    /// actually elapse. A cancellation before the timeout MUST always
    /// release the bond regardless of this flag; see §Phase 4 of the spec.
    #[serde(default)]
    pub slash_on_waiting_timeout: bool,
    /// How long (seconds) Mostro waits between payout-invoice retries
    /// when asking the winning counterparty for a bolt11. Used by Phase 3.
    #[serde(default = "default_payout_invoice_window_seconds")]
    pub payout_invoice_window_seconds: u64,
    /// Maximum number of payout-invoice retries before a bond transitions
    /// to `failed` state. Used by Phase 3.
    #[serde(default = "default_payout_max_retries")]
    pub payout_max_retries: u32,
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

impl Default for AntiAbuseBondSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            amount_pct: default_bond_amount_pct(),
            base_amount_sats: default_bond_base_amount(),
            apply_to: BondApplyTo::default(),
            slash_on_lost_dispute: false,
            slash_on_waiting_timeout: false,
            payout_invoice_window_seconds: default_payout_invoice_window_seconds(),
            payout_max_retries: default_payout_max_retries(),
        }
    }
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
#[serde(default)]
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
    /// Enable BOLT12 offer payout via LNDK (experimental)
    pub lndk_enabled: bool,
    /// LNDK gRPC host (must start with https://)
    #[serde(default = "default_lndk_grpc_host")]
    pub lndk_grpc_host: String,
    /// Path to LNDK TLS certificate (self-signed, generated on first run)
    pub lndk_cert_file: String,
    /// Path to the LND macaroon LNDK uses for payment authorization
    pub lndk_macaroon_file: String,
    /// Timeout for BOLT12 invoice fetch from the offer issuer (seconds)
    #[serde(default = "default_lndk_fetch_timeout")]
    pub lndk_fetch_invoice_timeout: u32,
    /// Fee limit for BOLT12 payments as percent. Falls back to mostro.max_routing_fee.
    pub lndk_fee_limit_percent: Option<f64>,
    /// Enable BIP-353 DNS resolution for `user@domain` payment addresses.
    /// Resolves via DNS-over-HTTPS to a DNSSEC-validated BOLT12 offer.
    /// Falls back to LNURL if resolution fails. Requires LNDK to be enabled.
    pub bip353_enabled: bool,
    /// DNS-over-HTTPS resolver URL for BIP-353 lookups.
    /// Must support the JSON API (RFC 8484). Default: Cloudflare.
    #[serde(default = "default_bip353_doh_resolver")]
    pub bip353_doh_resolver: String,
    /// Skip DNSSEC validation (AD flag check) for BIP-353 lookups.
    /// DANGER: only for regtest/dev. An attacker can redirect payments
    /// without DNSSEC validation.
    pub bip353_skip_dnssec: bool,
}

fn default_lndk_grpc_host() -> String {
    "https://127.0.0.1:7000".to_string()
}

fn default_lndk_fetch_timeout() -> u32 {
    60
}

fn default_bip353_doh_resolver() -> String {
    "https://1.1.1.1/dns-query".to_string()
}
/// Nostr configuration settings
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct NostrSettings {
    /// Nostr private key
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
        assert!(!cfg.slash_on_lost_dispute);
        assert!(!cfg.slash_on_waiting_timeout);
        assert_eq!(cfg.apply_to, BondApplyTo::Take);
        assert_eq!(cfg.amount_pct, 0.01);
        assert_eq!(cfg.base_amount_sats, 1_000);
        assert_eq!(cfg.payout_invoice_window_seconds, 300);
        assert_eq!(cfg.payout_max_retries, 5);
    }

    #[test]
    fn apply_to_predicates() {
        assert!(BondApplyTo::Take.applies_to_taker());
        assert!(!BondApplyTo::Take.applies_to_maker());
        assert!(!BondApplyTo::Create.applies_to_taker());
        assert!(BondApplyTo::Create.applies_to_maker());
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
slash_on_lost_dispute = true"#,
        )
        .expect("toml parses");
        assert_eq!(parsed.anti_abuse_bond.apply_to, BondApplyTo::Both);
        assert!(parsed.anti_abuse_bond.slash_on_lost_dispute);
    }
}
