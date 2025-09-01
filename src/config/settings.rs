use super::{DB_POOL, MOSTRO_CONFIG};
use crate::config::types::{
    DatabaseSettings, LightningSettings, MostroSettings, NostrSettings, RpcSettings,
};
use serde::Deserialize;
use std::sync::Arc;

// Mostro configuration settings struct
#[derive(Debug, Deserialize, Clone)]
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
}
