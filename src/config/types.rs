// File with the types for the configuration settings
// Initialize the types for the configuration settings
use serde::Deserialize;

// / Implement the TryFrom trait for each of the structs in Settings
// / This allows you to convert from Settings to each of the structs directly.
macro_rules! impl_try_from_settings {
    ($($ty:ty => $field:ident),*) => {
        $(
            impl TryFrom<super::Settings> for $ty {
                type Error = mostro_core::error::MostroError;

                fn try_from(_: super::Settings) -> Result<Self, Self::Error> {
                    Ok(crate::MOSTRO_CONFIG.get().unwrap().$field.clone())
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
/// Mostro configuration settings

#[derive(Debug, Deserialize, Default, Clone)]
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
}

// Macro call here to implement the TryFrom trait for each of the structs in Settings
impl_try_from_settings!(
    DatabaseSettings => database,
    LightningSettings => lightning,
    NostrSettings => nostr,
    MostroSettings => mostro
);
