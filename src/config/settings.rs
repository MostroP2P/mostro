use super::{DB_POOL, MOSTRO_CONFIG, NOSTR_KEYS};
use crate::config::secret::take_nsec_for_init;
use crate::config::types::{
    AntiAbuseBondSettings, CashuSettings, DatabaseSettings, EscrowMode, ExpirationSettings,
    LightningSettings, MostroSettings, NostrSettings, ReputationExportSettings,
    ReputationImportSettings, RpcSettings,
};
use crate::price::PriceSettings;
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use mostro_core::transport::Transport;
use nostr_sdk::prelude::Keys;
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
    /// Cashu escrow configuration (docs/cashu/, CF-1). Absent section ≡
    /// disabled (Lightning mode). Mutually exclusive with
    /// `anti_abuse_bond`; enforced at startup.
    #[serde(default)]
    pub cashu: Option<CashuSettings>,
    /// Multi-source price configuration (see `docs/PRICE_PROVIDERS.md`).
    /// Absent section ≡ legacy single-source behaviour (synthesised in
    /// Phase 1's migration).
    #[serde(default)]
    pub price: Option<PriceSettings>,
    /// Reputation import (docs/REPUTATION_PORTABILITY.md, phase 3). Absent
    /// section ≡ disabled.
    #[serde(default)]
    pub reputation_import: Option<ReputationImportSettings>,
    /// Reputation export (docs/REPUTATION_PORTABILITY.md, phase 4). Absent
    /// section ≡ disabled.
    #[serde(default)]
    pub reputation_export: Option<ReputationExportSettings>,
}

/// Initialize the global `MOSTRO_CONFIG` and `NOSTR_KEYS` structs.
pub fn init_mostro_settings(mut s: Settings) -> Result<(), MostroError> {
    let keys = take_nsec_for_init(&mut s.nostr)?;
    attach_reputation_issuer_keys(&mut s, &keys)?;
    NOSTR_KEYS.set(keys).map_err(|_| {
        MostroInternalErr(ServiceError::IOError(
            "Mostro nostr keys already initialized".to_string(),
        ))
    })?;
    MOSTRO_CONFIG.set(s).map_err(|_| {
        MostroInternalErr(ServiceError::IOError(
            "Mostro settings already initialized".to_string(),
        ))
    })?;
    Ok(())
}

/// Load the reputation issuer key when export is enabled: from the
/// environment variable the section names, a dedicated key that is neither
/// the node's own key nor one the node imports from (it would let a user
/// import their own reputation here and double it). Any of these stops the
/// boot.
pub(crate) fn attach_reputation_issuer_keys(
    s: &mut Settings,
    node: &Keys,
) -> Result<(), MostroError> {
    let trusted = s
        .reputation_import
        .as_ref()
        .map(ReputationImportSettings::trusted_keys)
        .unwrap_or_default();
    let Some(export) = s.reputation_export.as_mut().filter(|e| e.enabled) else {
        return Ok(());
    };
    let issuer = crate::config::secret::load_reputation_issuer_keys(&export.issuer_key_env)?;
    let fail = |reason: &str| {
        Err(MostroInternalErr(ServiceError::IOError(format!(
            "[reputation_export] {reason}"
        ))))
    };
    if issuer.public_key() == node.public_key() {
        return fail("the issuer key must be a dedicated key, not the node's own key");
    }
    if trusted.contains(&issuer.public_key()) {
        return fail(
            "the issuer key is also in [reputation_import]: a node never imports its own attestations",
        );
    }
    export.issuer_keys = Some(issuer);
    Ok(())
}

/// Parsed Mostro Nostr signing keys, initialized once at startup.
pub fn get_mostro_keys() -> Option<&'static Keys> {
    NOSTR_KEYS.get()
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
    /// block is absent (treated as disabled), and also when the global
    /// settings haven't been initialized yet — unlike the other accessors
    /// in this file, the bond gate is on the hot path of the take flow and
    /// must never panic in unit tests that don't bring up the full
    /// configuration.
    pub fn get_bond() -> Option<&'static AntiAbuseBondSettings> {
        MOSTRO_CONFIG.get()?.anti_abuse_bond.as_ref()
    }

    /// The reputation export settings, only when export is enabled; its
    /// `issuer_keys` were loaded at startup. Like [`Settings::get_bond`],
    /// never panics before the configuration is initialised.
    pub fn get_reputation_export() -> Option<&'static ReputationExportSettings> {
        MOSTRO_CONFIG
            .get()?
            .reputation_export
            .as_ref()
            .filter(|export| export.enabled)
    }

    /// The reputation import settings, only when import is enabled.
    pub fn get_reputation_import() -> Option<&'static ReputationImportSettings> {
        MOSTRO_CONFIG
            .get()?
            .reputation_import
            .as_ref()
            .filter(|import| import.enabled)
    }

    /// Wire transport for protocol messages. Falls back to the daemon
    /// default (`nip44`, protocol v2 — see `default_transport`) when the
    /// global settings haven't been initialized yet — `send_dm()` sits on
    /// every reply path and must degrade gracefully rather than panic in
    /// unit tests that don't bring up the full configuration, mirroring
    /// [`Settings::get_bond`].
    pub fn get_transport() -> Transport {
        Self::transport_or_default(MOSTRO_CONFIG.get())
    }

    /// Selection logic behind [`Settings::get_transport`], split out so the
    /// uninitialized (`None`) fallback is unit-testable without touching the
    /// process-wide `MOSTRO_CONFIG`.
    fn transport_or_default(settings: Option<&Settings>) -> Transport {
        settings
            .map(|s| s.mostro.transport)
            .unwrap_or_else(crate::config::types::default_transport)
    }

    /// Retrieve the multi-source price configuration from the global
    /// `MOSTRO_CONFIG`. Returns `None` when the `[price]` block is absent
    /// (Phase 1 synthesises a legacy default in that case) and also when the
    /// global settings haven't been initialized yet, so it never panics on
    /// the price hot path or in unit tests that don't bring up the full
    /// configuration — mirroring [`Settings::get_bond`].
    pub fn get_price() -> Option<&'static PriceSettings> {
        MOSTRO_CONFIG.get()?.price.as_ref()
    }

    /// True when the feature is configured AND explicitly enabled. This is
    /// the single gate every bond-related code path must check before
    /// running. Keeps the opt-in guarantee simple to audit. Returns
    /// `false` when settings haven't been initialized.
    pub fn is_bond_enabled() -> bool {
        Self::get_bond().is_some_and(|cfg| cfg.enabled)
    }

    /// Retrieve the Cashu escrow configuration from the global
    /// `MOSTRO_CONFIG`. Returns `None` when the `[cashu]` block is absent
    /// (treated as disabled) and also when the global settings haven't
    /// been initialized yet — the escrow-mode gate will sit on handler hot
    /// paths, so it must never panic in unit tests that don't bring up the
    /// full configuration, mirroring [`Settings::get_bond`].
    pub fn get_cashu() -> Option<&'static CashuSettings> {
        MOSTRO_CONFIG.get()?.cashu.as_ref()
    }

    /// True when the `[cashu]` block is present AND explicitly enabled.
    /// The single gate every Cashu code path must check. Returns `false`
    /// when settings haven't been initialized.
    pub fn is_cashu_enabled() -> bool {
        Self::get_cashu().is_some_and(|cfg| cfg.enabled)
    }

    /// The node-wide escrow mode (locked decision §4.1). `[cashu]` absent
    /// or disabled ⇒ `Lightning`. Nothing calls this on a hot path during
    /// the foundation milestone — CF-1 wires it to nothing at runtime.
    pub fn escrow_mode() -> EscrowMode {
        if Self::is_cashu_enabled() {
            EscrowMode::Cashu
        } else {
            EscrowMode::Lightning
        }
    }
}

#[cfg(test)]
mod tests {

    mod reputation_issuer_keys {
        use super::super::*;
        use crate::app::context::test_utils::test_settings;
        use crate::config::types::{ReputationImportSettings, ReputationIssuer};
        use nostr_sdk::prelude::{Keys, ToBech32};

        /// Each test sets its own variable, so parallel tests never race.
        fn settings_with(env: &str, value: Option<&str>) -> Settings {
            match value {
                Some(value) => std::env::set_var(env, value),
                None => std::env::remove_var(env),
            }
            let mut settings = test_settings();
            settings.reputation_export = Some(ReputationExportSettings {
                enabled: true,
                issuer_key_env: env.to_string(),
                ..Default::default()
            });
            settings
        }

        fn refusal(result: Result<(), MostroError>) -> String {
            match result {
                Err(MostroInternalErr(ServiceError::IOError(reason))) => reason,
                other => panic!("expected a refusal, got {other:?}"),
            }
        }

        #[test]
        fn loads_a_dedicated_key_from_the_named_variable() {
            let issuer = Keys::generate();
            let nsec = issuer.secret_key().to_bech32().unwrap();
            let mut settings = settings_with("MOSTRO_TEST_ISSUER_SK_OK", Some(&nsec));
            attach_reputation_issuer_keys(&mut settings, &Keys::generate()).unwrap();
            let export = settings.reputation_export.unwrap();
            assert_eq!(export.issuer_key(), Some(issuer.public_key()));
        }

        #[test]
        fn a_disabled_section_needs_no_key() {
            let mut settings = settings_with("MOSTRO_TEST_ISSUER_SK_OFF", None);
            settings.reputation_export.as_mut().unwrap().enabled = false;
            attach_reputation_issuer_keys(&mut settings, &Keys::generate()).unwrap();
            assert!(settings.reputation_export.unwrap().issuer_keys.is_none());
        }

        #[test]
        fn an_unset_empty_or_invalid_key_stops_the_boot() {
            let mut unset = settings_with("MOSTRO_TEST_ISSUER_SK_UNSET", None);
            assert!(
                refusal(attach_reputation_issuer_keys(&mut unset, &Keys::generate()))
                    .contains("is not set")
            );
            let mut empty = settings_with("MOSTRO_TEST_ISSUER_SK_EMPTY", Some("  "));
            assert!(
                refusal(attach_reputation_issuer_keys(&mut empty, &Keys::generate()))
                    .contains("is empty")
            );
            let mut bad = settings_with("MOSTRO_TEST_ISSUER_SK_BAD", Some("nsec1nope"));
            assert!(
                refusal(attach_reputation_issuer_keys(&mut bad, &Keys::generate()))
                    .contains("not a valid secret key")
            );
        }

        #[test]
        fn the_node_key_is_never_the_issuer_key() {
            let node = Keys::generate();
            let hex = node.secret_key().to_secret_hex();
            let mut settings = settings_with("MOSTRO_TEST_ISSUER_SK_NODE", Some(&hex));
            assert!(refusal(attach_reputation_issuer_keys(&mut settings, &node))
                .contains("not the node's own key"));
        }

        #[test]
        fn a_key_the_node_imports_from_is_never_its_issuer_key() {
            let issuer = Keys::generate();
            let hex = issuer.secret_key().to_secret_hex();
            let mut settings = settings_with("MOSTRO_TEST_ISSUER_SK_SELF", Some(&hex));
            settings.reputation_import = Some(ReputationImportSettings {
                enabled: true,
                issuers: vec![ReputationIssuer {
                    name: "self".to_string(),
                    keys: vec![issuer.public_key().to_hex()],
                }],
                ..Default::default()
            });
            assert!(refusal(attach_reputation_issuer_keys(
                &mut settings,
                &Keys::generate()
            ))
            .contains("[reputation_import]"));
        }
    }

    use super::*;
    use crate::app::context::test_utils::test_settings;

    /// Every test-side initializer in this crate installs a default-shaped
    /// `Settings` (no bond, no cashu, no price, expiration present), so
    /// these assertions hold regardless of which module's init wins the
    /// OnceLock race.
    fn init_test_settings() {
        let _ = MOSTRO_CONFIG.set(test_settings());
    }

    #[test]
    fn typed_getters_read_the_global_config() {
        init_test_settings();
        // Mostro / Lightning / RPC blocks are `Default` in every test
        // initializer — pin a representative field from each.
        assert_eq!(
            Settings::get_mostro().expiration_seconds,
            MostroSettings::default().expiration_seconds
        );
        assert_eq!(
            Settings::get_ln().payment_attempts,
            LightningSettings::default().payment_attempts
        );
        assert_eq!(Settings::get_rpc().port, RpcSettings::default().port);
        // Nostr/database blocks vary slightly per initializer — only
        // assert they are readable without panicking.
        let _ = Settings::get_nostr();
        let _ = Settings::get_db();
        assert!(Settings::get_expiration().is_some());
    }

    #[test]
    fn optional_blocks_absent_in_test_configuration() {
        init_test_settings();
        assert!(Settings::get_bond().is_none());
        assert!(!Settings::is_bond_enabled());
        assert!(Settings::get_cashu().is_none());
        assert!(!Settings::is_cashu_enabled());
        assert!(Settings::get_price().is_none());
        assert_eq!(Settings::escrow_mode(), EscrowMode::Lightning);
    }

    #[test]
    fn transport_falls_back_to_nip44_when_uninitialized() {
        assert_eq!(Settings::transport_or_default(None), Transport::Nip44Direct);
    }

    #[test]
    fn get_transport_reads_global_config() {
        init_test_settings();
        assert_eq!(Settings::get_transport(), Transport::Nip44Direct);
    }
}
