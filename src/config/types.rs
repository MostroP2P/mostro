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

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Database {
    pub url: String,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Lightning {
    pub lnd_cert_file: String,
    pub lnd_macaroon_file: String,
    pub lnd_grpc_host: String,
    pub invoice_expiration_window: u32,
    pub hold_invoice_cltv_delta: u32,
    pub hold_invoice_expiration_window: u32,
    pub payment_attempts: u32,
    pub payment_retries_interval: u32,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Nostr {
    pub nsec_privkey: String,
    pub relays: Vec<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Mostro {
    pub fee: f64,
    pub max_routing_fee: f64,
    pub max_order_amount: u32,
    pub min_payment_amount: u32,
    pub expiration_hours: u32,
    pub expiration_seconds: u32,
    pub user_rates_sent_interval_seconds: u32,
    pub max_expiration_days: u32,
    pub publish_relays_interval: u32,
    pub pow: u8,
    pub publish_mostro_info_interval: u32,
}

// Macro call here to implement the TryFrom trait for each of the structs in Settings
impl_try_from_settings!(
    Database => database,
    Lightning => lightning,
    Nostr => nostr,
    Mostro => mostro
);

