use crate::config::types::{DatabaseSettings, LightningSettings, MostroSettings, NostrSettings};
use crate::MOSTRO_CONFIG;
use serde::Deserialize;

// Mostro configuration settings struct
#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub database: DatabaseSettings,
    pub nostr: NostrSettings,
    pub mostro: MostroSettings,
    pub lightning: LightningSettings,
}

/// Initialize the global MOSTRO_CONFIG struct
pub fn init_mostro_settings(s: Settings) {
    MOSTRO_CONFIG
        .set(s)
        .expect("Failed to set Mostro global settings");
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
}
