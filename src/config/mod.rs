// Mostro module for configurataion settings
pub mod constants;
pub mod secret;
pub mod settings;
/// This module provides functionality to manage and initialize settings for the Mostro application.
/// It includes structures for database, lightning, Nostr, and Mostro settings, as well as functions to initialize and access these settings.
pub mod types;
pub mod util;
pub mod wizard;

// Mostro configuration module
// This module provides global configuration settings for the Mostro lightning configuration.
use crate::lightning::LnStatus;

// Synchronization primitives for thread safety -used for different global variables
use std::sync::{Arc, LazyLock, OnceLock};
use tokio::sync::RwLock;

// Re-export for convenience
pub use constants::{DEV_FEE_LIGHTNING_ADDRESS, MAX_DEV_FEE_PERCENTAGE, MIN_DEV_FEE_PERCENTAGE};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
pub use secret::{parse_mostro_keys, read_nsec_env_var, take_nsec_for_init};
pub use settings::{get_db_pool, get_mostro_keys, init_mostro_settings, Settings};
pub use types::{
    AntiAbuseBondSettings, BondApplyTo, DatabaseSettings, ExpirationSettings, LightningSettings,
    MostroSettings, NostrSettings,
};

// Global variables for Mostro configuration, Nostr client, Lightning status, and database pool
// almost all of them are initialized with OnceLock to ensure they are set only once
// They are shared across the application using Arc and Mutex/RwLock for thread safety
pub static MOSTRO_CONFIG: OnceLock<Settings> = OnceLock::new();
pub static NOSTR_KEYS: OnceLock<Keys> = OnceLock::new();
pub static NOSTR_CLIENT: OnceLock<Client> = OnceLock::new();
pub static LN_STATUS: OnceLock<LnStatus> = OnceLock::new();
pub static DB_POOL: OnceLock<Arc<sqlx::SqlitePool>> = OnceLock::new();

/// Global message queues for Mostro
/// This struct holds three queues:
/// - `queue_order_msg`: Holds messages related to orders.
/// - `queue_order_cantdo`: Holds messages that cannot be processed.
/// - `queue_order_rate`: Holds events related to user rates.
/// - `queue_restore_session_msg`: Holds messages related to restore session.
///
/// Each queue is wrapped in an `Arc<RwLock<>>` to allow safe concurrent access across tasks.
#[derive(Debug, Clone, Default)]
pub struct MessageQueues {
    pub queue_order_msg: Arc<RwLock<Vec<(Message, PublicKey)>>>,
    pub queue_order_cantdo: Arc<RwLock<Vec<(Message, PublicKey)>>>,
    pub queue_order_rate: Arc<RwLock<Vec<Event>>>,
    pub queue_restore_session_msg: Arc<RwLock<Vec<(Message, PublicKey)>>>,
}

pub static MESSAGE_QUEUES: LazyLock<MessageQueues> = LazyLock::new(MessageQueues::default);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::constants::DEV_FEE_AUDIT_EVENT_KIND;
    use mostro_core::prelude::{NOSTR_DISPUTE_EVENT_KIND, NOSTR_ORDER_EVENT_KIND};
    use secrecy::ExposeSecret;
    use serde::Deserialize;

    // Fake settings for the test
    const NOSTR_SETTINGS: &str = r#"[nostr]
                                    nsec_privkey = 'nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd'
                                    relays = ['wss://relay.damus.io','wss://relay.mostro.network']"#;

    const LIGHTNING_SETTINGS: &str = r#"[lightning]
                                            lnd_cert_file = '/home/user/.polar/networks/1/volumes/lnd/alice/tls.cert'
                                            lnd_macaroon_file = '/home/user/.polar/networks/1/volumes/lnd/alice/data/chain/bitcoin/regtest/admin.macaroon'
                                            lnd_grpc_host = 'https://127.0.0.1:10001'
                                            invoice_expiration_window = 3600
                                            hold_invoice_cltv_delta = 144
                                            hold_invoice_expiration_window = 300
                                            payment_attempts = 3
                                            payment_retries_interval = 60"#;

    const DATABASE_SETTINGS: &str = r#"[database]
                                            url = 'sqlite://mostro.db'"#;

    const MOSTRO_SETTINGS: &str = r#"[mostro]
                                            fee = 0
                                            max_routing_fee = 0.002
                                            max_order_amount = 1000000
                                            min_payment_amount = 100
                                            expiration_hours = 24
                                            max_expiration_days = 15
                                            expiration_seconds = 900
                                            user_rates_sent_interval_seconds = 3600
                                            publish_relays_interval = 60
                                            pow = 0
                                            publish_mostro_info_interval = 300
                                            bitcoin_price_api_url = "https://api.yadio.io"
                                            fiat_currencies_accepted = ['USD', 'EUR', 'ARS', 'CUP']
                                            max_orders_per_response = 10
                                            dev_fee_percentage = 0.30"#;

    const EXPIRATION_SETTINGS: &str = r#"[expiration]
                                            order_days = 45
                                            dispute_days = 120
                                            fee_audit_days = 400"#;

    const EXPIRATION_SETTINGS_MISSING: &str = "";

    // Stub structures for the test
    #[derive(Debug, Deserialize)]
    struct StubSettingsLightning {
        lightning: LightningSettings,
    }

    #[derive(Debug, Deserialize)]
    struct StubSettingsDatabase {
        database: DatabaseSettings,
    }

    #[derive(Debug, Deserialize)]
    struct StubSettingsNostr {
        nostr: NostrSettings,
    }

    #[derive(Debug, Deserialize)]
    struct StubSettingsMostro {
        mostro: MostroSettings,
    }

    #[derive(Debug, Deserialize)]
    struct StubSettingsExpiration {
        expiration: Option<ExpirationSettings>,
    }

    #[test]
    fn test_expiration_settings_present() {
        let parsed: StubSettingsExpiration =
            toml::from_str(EXPIRATION_SETTINGS).expect("Failed to deserialize");
        let expiration = parsed.expiration.expect("Expected expiration settings");

        assert_eq!(expiration.order_days, Some(45));
        assert_eq!(expiration.dispute_days, Some(120));
        assert_eq!(expiration.fee_audit_days, Some(400));
    }

    #[test]
    fn test_expiration_settings_absent() {
        let parsed: StubSettingsExpiration =
            toml::from_str(EXPIRATION_SETTINGS_MISSING).expect("Failed to deserialize");
        let expiration = parsed.expiration.unwrap_or_default();

        assert_eq!(
            expiration.get_expiration_for_kind(NOSTR_ORDER_EVENT_KIND),
            Some(30)
        );
        assert_eq!(
            expiration.get_expiration_for_kind(NOSTR_DISPUTE_EVENT_KIND),
            Some(90)
        );
        assert_eq!(
            expiration.get_expiration_for_kind(DEV_FEE_AUDIT_EVENT_KIND),
            Some(365)
        );
    }

    #[test]
    fn test_lighting_settings() {
        // Parse TOML content
        let lightning_settings: StubSettingsLightning =
            toml::from_str(LIGHTNING_SETTINGS).expect("Failed to deserialize");
        assert_eq!(
            lightning_settings.lightning.lnd_cert_file,
            "/home/user/.polar/networks/1/volumes/lnd/alice/tls.cert"
        );
        assert_eq!(lightning_settings.lightning.lnd_macaroon_file, "/home/user/.polar/networks/1/volumes/lnd/alice/data/chain/bitcoin/regtest/admin.macaroon");
        assert_eq!(
            lightning_settings.lightning.lnd_grpc_host,
            "https://127.0.0.1:10001"
        );
        assert_eq!(lightning_settings.lightning.invoice_expiration_window, 3600);
        assert_eq!(lightning_settings.lightning.hold_invoice_cltv_delta, 144);
        assert_eq!(
            lightning_settings.lightning.hold_invoice_expiration_window,
            300
        );
        assert_eq!(lightning_settings.lightning.payment_attempts, 3);
        assert_eq!(lightning_settings.lightning.payment_retries_interval, 60);
    }

    #[test]
    fn test_database_settings() {
        // Parse TOML content
        let database_settings: StubSettingsDatabase =
            toml::from_str(DATABASE_SETTINGS).expect("Failed to deserialize");
        assert_eq!(database_settings.database.url, "sqlite://mostro.db");
    }

    #[test]
    fn test_nostr_settings() {
        // Parse TOML content
        let nostr_settings: StubSettingsNostr =
            toml::from_str(NOSTR_SETTINGS).expect("Failed to deserialize");
        assert_eq!(
            nostr_settings.nostr.nsec_privkey.expose_secret(),
            "nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd"
        );
        assert_eq!(
            nostr_settings.nostr.relays,
            vec!["wss://relay.damus.io", "wss://relay.mostro.network"]
        );
    }

    #[test]
    fn test_mostro_settings() {
        // Parse TOML content
        let mostro_settings: StubSettingsMostro =
            toml::from_str(MOSTRO_SETTINGS).expect("Failed to deserialize");
        assert_eq!(mostro_settings.mostro.fee, 0.0);
        assert_eq!(mostro_settings.mostro.max_routing_fee, 0.002);
        assert_eq!(mostro_settings.mostro.max_order_amount, 1000000);
        assert_eq!(mostro_settings.mostro.min_payment_amount, 100);
        assert_eq!(mostro_settings.mostro.expiration_hours, 24);
        assert_eq!(mostro_settings.mostro.max_expiration_days, 15);
        assert_eq!(mostro_settings.mostro.expiration_seconds, 900);
        assert_eq!(
            mostro_settings.mostro.user_rates_sent_interval_seconds,
            3600
        );
        assert_eq!(mostro_settings.mostro.publish_relays_interval, 60);
        assert_eq!(mostro_settings.mostro.pow, 0);
        assert_eq!(mostro_settings.mostro.publish_mostro_info_interval, 300);
        assert_eq!(
            mostro_settings.mostro.bitcoin_price_api_url,
            "https://api.yadio.io"
        );
        assert_eq!(
            mostro_settings.mostro.fiat_currencies_accepted,
            vec!["USD", "EUR", "ARS", "CUP"]
        );
        assert_eq!(mostro_settings.mostro.max_orders_per_response, 10);
    }

    // Same as MOSTRO_SETTINGS but with `bitcoin_price_api_url` omitted — the
    // shape an operator who has migrated to a `[price]` block and deleted the
    // deprecated key would have (spec §10.1).
    const MOSTRO_SETTINGS_NO_PRICE_URL: &str = r#"[mostro]
                                            fee = 0
                                            max_routing_fee = 0.002
                                            max_order_amount = 1000000
                                            min_payment_amount = 100
                                            expiration_hours = 24
                                            max_expiration_days = 15
                                            expiration_seconds = 900
                                            user_rates_sent_interval_seconds = 3600
                                            publish_relays_interval = 60
                                            pow = 0
                                            publish_mostro_info_interval = 300
                                            fiat_currencies_accepted = ['USD', 'EUR', 'ARS', 'CUP']
                                            max_orders_per_response = 10
                                            dev_fee_percentage = 0.30"#;

    #[test]
    fn test_mostro_settings_without_bitcoin_price_api_url_defaults() {
        // A settings.toml that omits the deprecated key must still
        // deserialize (it is `#[serde(default)]`), falling back to the same
        // URL the `Default` impl uses.
        let parsed: StubSettingsMostro =
            toml::from_str(MOSTRO_SETTINGS_NO_PRICE_URL).expect("must deserialize without the key");
        assert_eq!(
            parsed.mostro.bitcoin_price_api_url, "https://api.yadio.io",
            "omitted legacy key must fall back to the default URL"
        );
    }

    #[test]
    fn test_legacy_synthesis_same_whether_key_present_or_defaulted() {
        // Legacy synthesis (`[price]` absent) must produce the same
        // single-yadio config whether the deprecated key was present at its
        // default value or omitted entirely (spec §10.1 byte-for-byte
        // compatibility).
        let omitted: StubSettingsMostro =
            toml::from_str(MOSTRO_SETTINGS_NO_PRICE_URL).expect("deserialize (omitted)");
        let present: StubSettingsMostro =
            toml::from_str(MOSTRO_SETTINGS).expect("deserialize (present)");

        let synth = |m: &MostroSettings| {
            crate::price::synthesise_legacy_price_settings(
                &m.bitcoin_price_api_url,
                m.exchange_rates_update_interval_seconds,
                m.publish_exchange_rates_to_nostr,
            )
        };
        let from_omitted = synth(&omitted.mostro);
        let from_present = synth(&present.mostro);

        // Exactly one provider (yadio) in both, with the default URL.
        for cfg in [&from_omitted, &from_present] {
            assert_eq!(cfg.providers.len(), 1);
            let yadio = cfg.providers.get("yadio").expect("yadio provider present");
            assert!(yadio.enabled);
            assert_eq!(yadio.url, "https://api.yadio.io");
        }
        // And the synthesised cadence / publish flag match across the two.
        assert_eq!(
            from_omitted.update_interval_seconds,
            from_present.update_interval_seconds
        );
        assert_eq!(from_omitted.publish_to_nostr, from_present.publish_to_nostr);
    }
}
