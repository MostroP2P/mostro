use super::{DB_POOL, MOSTRO_CONFIG};
use crate::config::types::{
    AntiAbuseBondSettings, DatabaseSettings, ExpirationSettings, LightningSettings, MostroSettings,
    NostrSettings, RpcSettings,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// Mostro configuration settings struct
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Settings {
    /// Url of database for Mostro
    pub database: DatabaseSettings,
    /// Nostr configuration settings
    pub nostr: NostrSettings,
    /// Mostro daemon configuration settings
    pub mostro: MostroSettings,
    /// Lightning configuration settings
    pub lightning: LightningSettings,
    /// RPC configuration settings
    pub rpc: RpcSettings,
    /// Event expiration configuration settings
    pub expiration: Option<ExpirationSettings>,
    /// Anti-abuse bond configuration (issue #711). Absent section ≡ disabled.
    #[serde(default)]
    pub anti_abuse_bond: Option<AntiAbuseBondSettings>,
}

/// Initialize the global MOSTRO_CONFIG struct
pub fn init_mostro_settings(s: Settings) {
    MOSTRO_CONFIG
        .set(s)
        .expect("Failed to set Mostro global settings");
}

/// Get database pool for Mostro db operations to share across the thread
pub fn get_db_pool() -> Arc<sqlx::SqlitePool> {
    DB_POOL.get().expect("No database pool found").clone()
}

impl Settings {
    /// This function retrieves the Lightning configuration from the global MOSTRO_CONFIG struct.
    pub fn get_ln() -> &'static LightningSettings {
        &MOSTRO_CONFIG
            .get()
            .expect("No Lightning settings found")
            .lightning
    }

    /// This function retrieves the Mostro configuration from the global MOSTRO_CONFIG struct.
    pub fn get_mostro() -> &'static MostroSettings {
        &MOSTRO_CONFIG
            .get()
            .expect("No Mostro settings found")
            .mostro
    }

    /// This function retrieves the Database configuration from the global MOSTRO_CONFIG struct.
    pub fn get_db() -> &'static DatabaseSettings {
        &MOSTRO_CONFIG
            .get()
            .expect("No Database settings found")
            .database
    }

    /// This function retrieves the Nostr configuration from the global MOSTRO_CONFIG struct.
    pub fn get_nostr() -> &'static NostrSettings {
        &MOSTRO_CONFIG.get().expect("No Nostr settings found").nostr
    }

    /// This function retrieves the RPC configuration from the global MOSTRO_CONFIG struct.
    pub fn get_rpc() -> &'static RpcSettings {
        &MOSTRO_CONFIG.get().expect("No RPC settings found").rpc
    }

    /// This function retrieves the Expiration configuration from the global MOSTRO_CONFIG struct.
    pub fn get_expiration() -> Option<&'static ExpirationSettings> {
        MOSTRO_CONFIG
            .get()
            .expect("No settings found")
            .expiration
            .as_ref()
    }

    /// This function retrieves the anti-abuse bond configuration from the
    /// global `MOSTRO_CONFIG`. Returns `None` when the `[anti_abuse_bond]`
    /// block is absent (treated as disabled).
    pub fn get_bond() -> Option<&'static AntiAbuseBondSettings> {
        MOSTRO_CONFIG
            .get()
            .expect("No settings found")
            .anti_abuse_bond
            .as_ref()
    }

    /// True when the feature is configured AND explicitly enabled. This is
    /// the single gate every bond-related code path must check before
    /// running. Keeps the opt-in guarantee simple to audit.
    pub fn is_bond_enabled() -> bool {
        Self::get_bond().is_some_and(|cfg| cfg.enabled)
    }
}
