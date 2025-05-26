use crate::config::types::{DatabaseSettings, LightningSettings, MostroSettings, NostrSettings};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use nostr_sdk::prelude::*;
use mostro_core::prelude::*;
use super::{MOSTRO_CONFIG, DB_POOL};

/// Message queues for Mostro:
/// - `queue_order_msg`: Holds messages related to orders.
/// - `queue_order_cantdo`: Holds messages that cannot be processed.
/// - `queue_order_rate`: Holds events related to user rates.
#[derive(Debug, Clone, Default)]
pub struct MessageQueues {
    pub queue_order_msg: Arc<Mutex<Vec<(Message, PublicKey)>>>,
    pub queue_order_cantdo: Arc<Mutex<Vec<(Message, PublicKey)>>>,
    pub queue_order_rate: Arc<Mutex<Vec<Event>>>,
}


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
}
