// Mostro module for configurataion settings
pub mod settings;
/// This module provides functionality to manage and initialize settings for the Mostro application.
/// It includes structures for database, lightning, Nostr, and Mostro settings, as well as functions to initialize and access these settings.
pub mod types;
pub mod util;

// Re-export for convenience
pub use settings::{init_mostro_settings, Settings};
pub use types::{DatabaseSettings, LightningSettings, MostroSettings, NostrSettings};

#[cfg(test)]
mod tests {
    use super::*;
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
                                            max_routing_fee = 0.001
                                            max_order_amount = 1000000
                                            min_payment_amount = 100
                                            expiration_hours = 24
                                            max_expiration_days = 15
                                            expiration_seconds = 900
                                            user_rates_sent_interval_seconds = 3600
                                            publish_relays_interval = 60
                                            pow = 0
                                            publish_mostro_info_interval = 300"#;

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
            nostr_settings.nostr.nsec_privkey,
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
        assert_eq!(mostro_settings.mostro.max_routing_fee, 0.001);
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
    }
}
