// File with the types for the configuration settings
// Initialize the types for the configuration settings
use crate::config::constants::DEV_FEE_AUDIT_EVENT_KIND;
use crate::config::MOSTRO_CONFIG;
use mostro_core::prelude::*;
use serde::Deserialize;

/// Event expiration configuration settings
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ExpirationSettings {
    /// Order events (kind 38383) expiration in days
    pub order_days: Option<u32>,
    /// Rating events (kind 38384) expiration in days
    pub rating_days: Option<u32>,
    /// Dispute events (kind 38386) expiration in days
    pub dispute_days: Option<u32>,
    /// Fee audit events (kind 8383) expiration in days
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
#[derive(Debug, Deserialize, Default, Clone)]
pub struct DatabaseSettings {
    /// Database connection URL (e.g., "postgres://user:pass@localhost/dbname")  
    pub url: String,
}
/// Lightning configuration settings
#[derive(Debug, Deserialize, Default, Clone)]
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
#[derive(Debug, Deserialize, Default, Clone)]
pub struct NostrSettings {
    /// Nostr private key
    pub nsec_privkey: String,
    /// Nostr relays list
    pub relays: Vec<String>,
}
/// RPC configuration settings
#[derive(Debug, Deserialize, Clone)]
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

#[derive(Debug, Deserialize, Clone)]
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
    /// NIP-01 kind 0 metadata: human-readable name for this Mostro instance
    pub name: Option<String>,
    /// NIP-01 kind 0 metadata: short description of this Mostro instance
    pub about: Option<String>,
    /// NIP-01 kind 0 metadata: URL to avatar image (recommended max 128x128px)
    pub picture: Option<String>,
    /// NIP-01 kind 0 metadata: operator website URL
    pub website: Option<String>,
}

impl Default for MostroSettings {
    fn default() -> Self {
        Self {
            fee: 0.0,
            max_routing_fee: 0.001,
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
