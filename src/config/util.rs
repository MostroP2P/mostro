/// Utility functions for the config module
/// This module provides utility functions for the config module.
/// It includes functions to initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// It also includes functions to add a trailing slash to a path if it doesn't already have one.
use crate::cashu::mint_policy::normalize_mint_url;
use crate::config::constants::{
    ENV_FILENAME, LND_DEFAULT_HOLD_EXPIRY_DELTA, MAX_DEV_FEE_PERCENTAGE,
    MAX_REPUTATION_LIFETIME_SECONDS, MIN_DEV_FEE_PERCENTAGE, MIN_ESCROW_DEADLINE_HEADROOM_BLOCKS,
    MIN_ESCROW_DEADLINE_MARGIN_BLOCKS,
};
use crate::config::secret::read_nsec_env_var;
use crate::config::wizard;
use crate::config::{get_mostro_keys, init_mostro_settings, Settings};
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use nostr_sdk::prelude::ToBech32;
use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use zeroize::Zeroizing;

const DB_FILENAME: &str = "mostro.db";

/// Loads the optional `<settings_dir>/.env` file so that values placed there
/// become available through `std::env::var`. Variables already set in the
/// process environment take precedence and are never overwritten.
///
/// Loading errors (malformed file, permission denied, ...) are logged as
/// warnings instead of being silently swallowed, so misconfigured deployments
/// surface the real root cause at startup rather than failing later with an
/// unrelated empty-key error.
fn load_env_file(settings_dir: &std::path::Path) {
    let env_file = settings_dir.join(ENV_FILENAME);
    if !env_file.exists() {
        return;
    }
    if let Err(e) = dotenvy::from_path(&env_file) {
        tracing::warn!(
            "Failed to load environment file {}: {}. Falling back to settings.toml.",
            env_file.display(),
            e
        );
    }
}

/// Log the node's pubkey, then that the settings loaded. Nothing is logged
/// before the settings on a normal start, so the node's identity is the first
/// line: an operator can match the logs to the pubkey clients connect to.
fn log_settings_loaded() {
    if let Some(keys) = get_mostro_keys() {
        let pubkey = keys.public_key();
        match pubkey.to_bech32() {
            Ok(npub) => tracing::info!("Mostro pubkey: {npub} (hex {})", pubkey.to_hex()),
            Err(_) => tracing::info!("Mostro pubkey: {}", pubkey.to_hex()),
        }
    }
    tracing::info!("Settings correctly loaded!");
}

/// If the `MOSTRO_NSEC_PRIVKEY` environment variable is set to a non-empty
/// value, override the nsec loaded from `settings.toml`. Whitespace is
/// trimmed; blank values are ignored so the TOML stays the fallback.
fn apply_nsec_env_override(settings: &mut Settings) {
    if let Some(nsec) = read_nsec_env_var() {
        settings.nostr.nsec_privkey = nsec;
    }
}

/// Validates Mostro settings on startup
fn validate_mostro_settings(settings: &Settings) -> Result<(), MostroError> {
    let dev_fee = settings.mostro.dev_fee_percentage;

    // Validate dev_fee_percentage range
    if dev_fee < MIN_DEV_FEE_PERCENTAGE {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "dev_fee_percentage ({}) is below minimum ({})",
            dev_fee, MIN_DEV_FEE_PERCENTAGE
        ))));
    }

    if dev_fee > MAX_DEV_FEE_PERCENTAGE {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "dev_fee_percentage ({}) exceeds maximum ({})",
            dev_fee, MAX_DEV_FEE_PERCENTAGE
        ))));
    }

    validate_cashu_settings(
        settings.cashu.as_ref(),
        settings
            .anti_abuse_bond
            .as_ref()
            .is_some_and(|bond| bond.enabled),
    )?;

    validate_serbero_pubkey(settings.mostro.serbero_pubkey.as_deref())?;

    validate_escrow_deadline_margin(
        &settings.lightning,
        settings.cashu.as_ref().is_some_and(|cashu| cashu.enabled),
    )?;

    if let Some(import) = settings.reputation_import.as_ref() {
        validate_reputation_import(import)?;
    }

    Ok(())
}

/// `escrow_deadline_margin_blocks` must be at least
/// [`MIN_ESCROW_DEADLINE_MARGIN_BLOCKS`] (LND's default `holdexpirydelta`
/// plus headroom) and below `hold_invoice_cltv_delta`. Below the minimum,
/// LND can refund the seller before the escrow-deadline guardian acts; at
/// or above the CLTV delta, the guardian acts within a few blocks of the
/// escrow being paid. Neither fails at runtime, so both stop the daemon at
/// load. The minimum assumes LND's default `holdexpirydelta`: a node that
/// raised it needs a larger margin, which mostrod cannot check yet (#1058).
/// Skipped in Cashu mode, which runs no guardian and has no hold invoice.
fn validate_escrow_deadline_margin(
    lightning: &crate::config::types::LightningSettings,
    cashu_enabled: bool,
) -> Result<(), MostroError> {
    if cashu_enabled {
        return Ok(());
    }

    let margin = lightning.escrow_deadline_margin_blocks;
    let cltv_delta = lightning.hold_invoice_cltv_delta;

    if margin < MIN_ESCROW_DEADLINE_MARGIN_BLOCKS {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "escrow_deadline_margin_blocks ({margin}) must be at least \
             {MIN_ESCROW_DEADLINE_MARGIN_BLOCKS} (LND's default \
             invoices.holdexpirydelta {LND_DEFAULT_HOLD_EXPIRY_DELTA} plus \
             {MIN_ESCROW_DEADLINE_HEADROOM_BLOCKS} blocks of headroom): below it \
             LND can refund the escrow before mostrod acts. If LND runs with a \
             higher holdexpirydelta, the margin must exceed that value by the \
             same headroom; mostrod does not check it"
        ))));
    }

    if margin >= cltv_delta {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "escrow_deadline_margin_blocks ({margin}) must be below \
             hold_invoice_cltv_delta ({cltv_delta}): at or above it mostrod \
             acts on a trade within a few blocks of the escrow being paid, \
             leaving the buyer no time to send the fiat"
        ))));
    }

    Ok(())
}

/// `[reputation_import]`: every issuer has a non-empty, unique name and at
/// least one key; every key parses (npub or hex) and belongs to one entry
/// only; the lifetime cap is positive. Checked even while disabled, so
/// turning the section on cannot surface a typo later. A key in two entries
/// would let one source account be imported twice, once under each name.
fn validate_reputation_import(
    import: &crate::config::types::ReputationImportSettings,
) -> Result<(), MostroError> {
    let fail = |reason: String| {
        Err(MostroInternalErr(ServiceError::IOError(format!(
            "[reputation_import] {reason}"
        ))))
    };
    if import.max_lifetime_seconds == 0 {
        return fail("max_lifetime_seconds must be greater than 0".to_string());
    }
    if import.max_lifetime_seconds > MAX_REPUTATION_LIFETIME_SECONDS {
        return fail(format!(
            "max_lifetime_seconds ({}) exceeds the maximum ({MAX_REPUTATION_LIFETIME_SECONDS}, 30 days)",
            import.max_lifetime_seconds
        ));
    }
    let mut names = std::collections::HashSet::new();
    let mut owners: std::collections::HashMap<nostr_sdk::prelude::PublicKey, &str> =
        std::collections::HashMap::new();
    for issuer in &import.issuers {
        let name = issuer.name.trim();
        if name.is_empty() {
            return fail("an issuer has an empty name".to_string());
        }
        // The name is the deduplication key and is recorded as written, so
        // it must not differ from its trimmed form.
        if name != issuer.name {
            return fail(format!(
                "issuer name `{}` has leading or trailing spaces",
                issuer.name
            ));
        }
        if !names.insert(name) {
            return fail(format!("issuer name `{name}` is used twice"));
        }
        if issuer.keys.is_empty() {
            return fail(format!("issuer `{name}` has no keys"));
        }
        for key in &issuer.keys {
            let Ok(parsed) = nostr_sdk::prelude::PublicKey::parse(key.trim()) else {
                return fail(format!("issuer `{name}` has an invalid key `{key}`"));
            };
            if let Some(other) = owners.insert(parsed, name) {
                return fail(format!(
                    "key `{key}` belongs to both `{other}` and `{name}`; a key belongs to one issuer only"
                ));
            }
        }
    }
    Ok(())
}

/// `serbero_pubkey`, when set, must be an npub or a hex public key. Checked at
/// load so a typo stops the daemon instead of silently running without the
/// assistant it was configured with. A blank value means none, like a blank
/// `MOSTRO_NSEC_PRIVKEY`.
fn validate_serbero_pubkey(serbero_pubkey: Option<&str>) -> Result<(), MostroError> {
    let Some(key) = serbero_pubkey.filter(|key| !key.trim().is_empty()) else {
        return Ok(());
    };
    nostr_sdk::prelude::PublicKey::parse(key.trim()).map_err(|_| {
        MostroInternalErr(ServiceError::IOError(format!(
            "serbero_pubkey ({key}) is not a valid npub or hex public key"
        )))
    })?;
    Ok(())
}

/// Validate the `[cashu]` block (Cashu foundation CF-1,
/// `docs/cashu/01-fundamentals.md` §6). Standalone so it is unit-testable
/// without building a full `Settings`.
///
/// Rules (all startup-fatal, so the daemon refuses to boot rather than
/// silently misbehave):
/// - `cashu.enabled` and `anti_abuse_bond.enabled` are mutually exclusive
///   (locked decision §4.5).
/// - When enabled, every `mint_urls` entry must be a usable `http`/`https`
///   mint URL ([`normalize_mint_url`]). An empty list is valid: the node
///   accepts any mint (issue #1046).
/// - When enabled, `escrow_locktime_days >= 1` (the seller-recovery
///   locktime floor of Track A §4B cannot be zero).
fn validate_cashu_settings(
    cashu: Option<&crate::config::types::CashuSettings>,
    bond_enabled: bool,
) -> Result<(), MostroError> {
    let Some(cashu) = cashu else {
        return Ok(());
    };
    if !cashu.enabled {
        return Ok(());
    }

    if bond_enabled {
        return Err(MostroInternalErr(ServiceError::IOError(
            "cashu.enabled and anti_abuse_bond.enabled are mutually exclusive: \
             a node runs bonds or Cashu escrow, never both"
                .to_string(),
        )));
    }

    for mint_url in &cashu.mint_urls {
        normalize_mint_url(mint_url).map_err(|reason| {
            MostroInternalErr(ServiceError::IOError(format!(
                "cashu.mint_urls entry {mint_url:?} {reason}"
            )))
        })?;
    }

    if cashu.escrow_locktime_days < 1 {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "cashu.escrow_locktime_days ({}) must be >= 1",
            cashu.escrow_locktime_days
        ))));
    }

    Ok(())
}

/// Initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// Checks if the directory already exists, and if not, creates it and writes the template file.
/// If a custom config path is provided, it uses that instead of the default `~/.mostro` directory.
pub fn init_configuration_file(config_path: Option<String>) -> Result<(), MostroError> {
    let settings_dir = if let Some(user_path) = config_path {
        PathBuf::from(user_path)
    } else {
        let home_dir = dirs::home_dir().ok_or_else(|| {
            MostroInternalErr(ServiceError::IOError(
                "Could not find home directory".to_string(),
            ))
        })?;
        let package_name = env!("CARGO_PKG_NAME");
        home_dir.join(format!(".{}", package_name))
    };

    // Check if /.mostro directory exists
    if !settings_dir.exists() {
        std::fs::create_dir_all(&settings_dir)
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
    }

    // Load `<settings_dir>/.env` so MOSTRO_NSEC_PRIVKEY (and any future env
    // overrides) can be read from it. Real env vars keep precedence.
    load_env_file(&settings_dir);

    let config_file_path = settings_dir.join("settings.toml");

    if !config_file_path.exists() {
        let mut settings = if std::io::stdin().is_terminal() {
            // Interactive: show setup menu (wizard or manual template)
            wizard::run_setup_menu(&settings_dir, &config_file_path)?
        } else {
            // Non-interactive (Docker, CI, systemd): copy template and exit
            std::fs::write(&config_file_path, include_bytes!("../../settings.tpl.toml"))
                .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
            println!(
                "Created settings file from template at {} - Edit it to configure your Mostro instance",
                config_file_path.display()
            );
            std::process::exit(0);
        };

        apply_nsec_env_override(&mut settings);
        validate_mostro_settings(&settings)?;
        init_mostro_settings(settings)?;
        log_settings_loaded();
        return Ok(());
    }

    // Read the file content into a zeroizing buffer so TOML plaintext is wiped
    // after parsing.
    let contents = Zeroizing::new(
        fs::read_to_string(&config_file_path)
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?,
    );

    // Parse TOML content
    let mut settings: Settings = toml::from_str(&contents)
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    // Apply MOSTRO_NSEC_PRIVKEY override before validation so an empty TOML
    // value is fine when the env var is set.
    apply_nsec_env_override(&mut settings);

    // Validate settings before initializing
    validate_mostro_settings(&settings)?;

    // Override database URL
    settings.database.url = format!("sqlite://{}", settings_dir.join(DB_FILENAME).display());

    // Initialize the global settings variable
    init_mostro_settings(settings)?;

    log_settings_loaded();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::constants::NSEC_ENV_VAR;
    use crate::config::types::{
        DatabaseSettings, LightningSettings, MostroSettings, NostrSettings, RpcSettings,
    };
    use secrecy::{ExposeSecret, SecretString};
    use std::sync::Mutex;

    // Tests that read/write MOSTRO_NSEC_PRIVKEY must run serially because the
    // process environment is shared across threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that saves the current value of an env var and restores it
    /// on drop, so tests don't leak state into each other.
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn new(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, previous }
        }

        fn set(&self, value: &str) {
            std::env::set_var(self.key, value);
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(val) => std::env::set_var(self.key, val),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn make_settings(nsec: &str) -> Settings {
        Settings {
            database: DatabaseSettings::default(),
            lightning: LightningSettings::default(),
            nostr: NostrSettings {
                nsec_privkey: SecretString::from(nsec.to_owned()),
                relays: vec!["wss://relay.test".to_string()],
            },
            mostro: MostroSettings::default(),
            rpc: RpcSettings::default(),
            expiration: None,
            anti_abuse_bond: None,
            cashu: None,
            price: None,
            reputation_import: None,
            payer_history: None,
        }
    }

    #[test]
    fn serbero_pubkey_accepts_npub_hex_or_nothing() {
        let key = nostr_sdk::prelude::Keys::generate().public_key();
        assert!(validate_serbero_pubkey(None).is_ok());
        assert!(validate_serbero_pubkey(Some("  ")).is_ok());
        assert!(validate_serbero_pubkey(Some(&key.to_hex())).is_ok());
        assert!(validate_serbero_pubkey(Some(&key.to_bech32().unwrap())).is_ok());
    }

    #[test]
    fn serbero_pubkey_rejects_a_malformed_key() {
        for bad in ["npub1notakey", "not-a-key", "abc123"] {
            assert!(
                validate_serbero_pubkey(Some(bad)).is_err(),
                "{bad:?} must not load"
            );
        }
    }

    #[test]
    fn env_var_overrides_toml_nsec() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvVarGuard::new(NSEC_ENV_VAR);
        guard.set("nsec_from_env");

        let mut settings = make_settings("nsec_from_toml");
        apply_nsec_env_override(&mut settings);

        assert_eq!(settings.nostr.nsec_privkey.expose_secret(), "nsec_from_env");
    }

    #[test]
    fn empty_env_var_falls_back_to_toml() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvVarGuard::new(NSEC_ENV_VAR);
        guard.set("");

        let mut settings = make_settings("nsec_from_toml");
        apply_nsec_env_override(&mut settings);

        assert_eq!(
            settings.nostr.nsec_privkey.expose_secret(),
            "nsec_from_toml"
        );
    }

    #[test]
    fn no_env_var_keeps_toml() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::new(NSEC_ENV_VAR);

        let mut settings = make_settings("nsec_from_toml");
        apply_nsec_env_override(&mut settings);

        assert_eq!(
            settings.nostr.nsec_privkey.expose_secret(),
            "nsec_from_toml"
        );
    }

    #[test]
    fn whitespace_only_env_is_ignored() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvVarGuard::new(NSEC_ENV_VAR);
        guard.set("   \t  ");

        let mut settings = make_settings("nsec_from_toml");
        apply_nsec_env_override(&mut settings);

        assert_eq!(
            settings.nostr.nsec_privkey.expose_secret(),
            "nsec_from_toml"
        );
    }

    #[test]
    fn env_guard_restores_preexisting_value_on_drop() {
        // When the env var already held a value, the guard must restore that
        // exact value on drop (the `Some(previous)` restore arm), not leave
        // the test's override leaking into sibling tests.
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var(NSEC_ENV_VAR, "preexisting_value");
        {
            let guard = EnvVarGuard::new(NSEC_ENV_VAR);
            guard.set("temporary_override");
            assert_eq!(
                std::env::var(NSEC_ENV_VAR).as_deref(),
                Ok("temporary_override")
            );
        }
        // Drop restored the original value.
        assert_eq!(
            std::env::var(NSEC_ENV_VAR).as_deref(),
            Ok("preexisting_value")
        );
        std::env::remove_var(NSEC_ENV_VAR);
    }

    #[test]
    fn env_var_value_is_trimmed() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvVarGuard::new(NSEC_ENV_VAR);
        guard.set("  nsec_from_env  ");

        let mut settings = make_settings("nsec_from_toml");
        apply_nsec_env_override(&mut settings);

        assert_eq!(settings.nostr.nsec_privkey.expose_secret(), "nsec_from_env");
    }

    #[test]
    fn toml_parses_without_nsec_privkey_field() {
        // Operators who rely exclusively on MOSTRO_NSEC_PRIVKEY should be able
        // to omit nsec_privkey from settings.toml entirely.
        let toml_without_nsec = r#"relays = ["wss://relay.test"]"#;
        let nostr: NostrSettings =
            toml::from_str(toml_without_nsec).expect("nsec_privkey should be optional in TOML");
        assert!(nostr.nsec_privkey.expose_secret().is_empty());
        assert_eq!(nostr.relays, vec!["wss://relay.test"]);
    }
}

#[cfg(test)]
mod reputation_import_validation_tests {
    use super::*;
    use crate::config::types::{ReputationImportSettings, ReputationIssuer};
    use nostr_sdk::prelude::{Keys, PublicKey, ToBech32};

    fn import(issuers: Vec<(&str, Vec<String>)>) -> ReputationImportSettings {
        ReputationImportSettings {
            enabled: true,
            issuers: issuers
                .into_iter()
                .map(|(name, keys)| ReputationIssuer {
                    name: name.to_string(),
                    keys,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn reason(result: Result<(), MostroError>) -> String {
        match result {
            Err(MostroInternalErr(ServiceError::IOError(reason))) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn named_issuers_with_npub_or_hex_keys_are_valid() {
        let (a, b, c) = (Keys::generate(), Keys::generate(), Keys::generate());
        let settings = import(vec![
            ("lnp2pbot", vec![a.public_key().to_bech32().unwrap()]),
            (
                "other-mostro",
                vec![b.public_key().to_hex(), c.public_key().to_hex()],
            ),
        ]);
        assert!(validate_reputation_import(&settings).is_ok());
        assert!(validate_reputation_import(&ReputationImportSettings::default()).is_ok());
    }

    #[test]
    fn a_key_in_two_entries_is_refused_even_spelled_differently() {
        let key = Keys::generate().public_key();
        let settings = import(vec![
            ("lnp2pbot", vec![key.to_hex()]),
            ("renamed", vec![key.to_bech32().unwrap()]),
        ]);
        assert!(reason(validate_reputation_import(&settings)).contains("one issuer only"));
    }

    #[test]
    fn names_must_be_present_and_unique() {
        let key = || vec![Keys::generate().public_key().to_hex()];
        assert!(
            reason(validate_reputation_import(&import(vec![(" ", key())]))).contains("empty name")
        );
        assert!(reason(validate_reputation_import(&import(vec![
            ("lnp2pbot", key()),
            ("lnp2pbot", key()),
        ])))
        .contains("used twice"));
    }

    /// The name is what imports are deduplicated on, so it is kept exactly as
    /// written: a padded name would be recorded padded and later clash with
    /// the trimmed one at the boot check.
    #[test]
    fn a_name_with_surrounding_spaces_is_refused() {
        let key = || vec![Keys::generate().public_key().to_hex()];
        for name in [" lnp2pbot", "lnp2pbot ", "\tlnp2pbot"] {
            assert!(
                reason(validate_reputation_import(&import(vec![(name, key())]))).contains("spaces"),
                "{name:?} must be refused"
            );
        }
    }

    #[test]
    fn a_key_with_surrounding_spaces_is_read_trimmed() {
        let key = Keys::generate().public_key();
        let settings = import(vec![("lnp2pbot", vec![format!(" {} ", key.to_hex())])]);
        assert!(validate_reputation_import(&settings).is_ok());
        assert_eq!(settings.trusted_keys(), vec![key]);
        assert_eq!(settings.issuer_for(&key), Some(&settings.issuers[0]));
    }

    #[test]
    fn keys_must_be_present_and_parse() {
        assert!(
            reason(validate_reputation_import(&import(vec![("a", vec![])]))).contains("no keys")
        );
        assert!(reason(validate_reputation_import(&import(vec![(
            "a",
            vec!["npub1nope".to_string()]
        )])))
        .contains("invalid key"));
    }

    #[test]
    fn the_lifetime_cap_must_be_positive_even_while_disabled() {
        let settings = ReputationImportSettings {
            max_lifetime_seconds: 0,
            ..Default::default()
        };
        assert!(reason(validate_reputation_import(&settings)).contains("max_lifetime_seconds"));
    }

    /// A cap of years would defeat its purpose; an extra digit on the
    /// 7-day default (70 days) must not load silently.
    #[test]
    fn the_lifetime_cap_is_at_most_thirty_days() {
        let thirty_days = MAX_REPUTATION_LIFETIME_SECONDS;
        let cap = |max_lifetime_seconds| ReputationImportSettings {
            max_lifetime_seconds,
            ..Default::default()
        };
        assert!(validate_reputation_import(&cap(thirty_days)).is_ok());
        for too_long in [thirty_days + 1, 6_048_000, u64::MAX] {
            assert!(
                reason(validate_reputation_import(&cap(too_long))).contains("max_lifetime_seconds"),
                "{too_long} must be refused"
            );
        }
    }

    #[test]
    fn an_empty_section_is_disabled() {
        #[derive(serde::Deserialize)]
        struct Stub {
            reputation_import: ReputationImportSettings,
        }
        let stub: Stub = toml::from_str("[reputation_import]\n").unwrap();
        assert_eq!(stub.reputation_import, ReputationImportSettings::default());
        assert!(!stub.reputation_import.enabled);
    }

    #[test]
    fn the_section_parses_from_toml_with_its_defaults() {
        #[derive(serde::Deserialize)]
        struct Stub {
            reputation_import: ReputationImportSettings,
        }
        let key = Keys::generate().public_key().to_hex();
        let stub: Stub = toml::from_str(&format!(
            "[reputation_import]\nenabled = true\n\n[[reputation_import.issuers]]\nname = \"lnp2pbot\"\nkeys = [\"{key}\"]\n"
        ))
        .unwrap();
        assert!(stub.reputation_import.enabled);
        assert_eq!(stub.reputation_import.max_lifetime_seconds, 604_800);
        assert_eq!(stub.reputation_import.issuers[0].name, "lnp2pbot");
        assert_eq!(
            stub.reputation_import
                .issuer_for(&PublicKey::from_hex(&key).unwrap()),
            Some(&stub.reputation_import.issuers[0])
        );
    }
}

#[cfg(test)]
mod cashu_validation_tests {
    use super::*;
    use crate::config::types::CashuSettings;

    fn enabled(mint_url: &str, days: u32) -> CashuSettings {
        CashuSettings {
            enabled: true,
            mint_urls: vec![mint_url.to_string()],
            escrow_locktime_days: days,
        }
    }

    #[test]
    fn absent_block_is_valid_regardless_of_bonds() {
        assert!(validate_cashu_settings(None, false).is_ok());
        assert!(validate_cashu_settings(None, true).is_ok());
    }

    #[test]
    fn disabled_block_is_valid_even_with_bonds() {
        let cashu = CashuSettings::default();
        assert!(validate_cashu_settings(Some(&cashu), true).is_ok());
    }

    #[test]
    fn rejects_cashu_and_bonds_together() {
        // Locked decision §4.5: a node runs bonds or Cashu, never both.
        let cashu = enabled("https://mint.example.com", 15);
        assert!(validate_cashu_settings(Some(&cashu), true).is_err());
    }

    #[test]
    fn accepts_valid_enabled_config() {
        let cashu = enabled("https://mint.example.com", 15);
        assert!(validate_cashu_settings(Some(&cashu), false).is_ok());
        let cashu_http = enabled("http://localhost:3338", 1);
        assert!(validate_cashu_settings(Some(&cashu_http), false).is_ok());
    }

    #[test]
    fn rejects_empty_or_malformed_mint_url() {
        assert!(validate_cashu_settings(Some(&enabled("", 15)), false).is_err());
        assert!(validate_cashu_settings(Some(&enabled("not a url", 15)), false).is_err());
    }

    #[test]
    fn accepts_an_empty_mint_list() {
        // No restriction: the node accepts any mint (issue #1046).
        let cashu = CashuSettings {
            enabled: true,
            ..CashuSettings::default()
        };
        assert!(validate_cashu_settings(Some(&cashu), false).is_ok());
    }

    #[test]
    fn accepts_several_valid_mints() {
        let cashu = CashuSettings {
            enabled: true,
            mint_urls: vec![
                "https://mint.example.com".to_string(),
                "http://localhost:3338".to_string(),
            ],
            ..CashuSettings::default()
        };
        assert!(validate_cashu_settings(Some(&cashu), false).is_ok());
    }

    #[test]
    fn rejects_a_list_with_one_bad_entry() {
        let cashu = CashuSettings {
            enabled: true,
            mint_urls: vec![
                "https://mint.example.com".to_string(),
                "ftp://mint.example.com".to_string(),
            ],
            ..CashuSettings::default()
        };
        let err = validate_cashu_settings(Some(&cashu), false).expect_err("bad entry");
        assert!(err.to_string().contains("ftp://mint.example.com"));
    }

    #[test]
    fn rejects_non_http_scheme() {
        let cashu = enabled("ftp://mint.example.com", 15);
        assert!(validate_cashu_settings(Some(&cashu), false).is_err());
        let cashu_ws = enabled("wss://mint.example.com", 15);
        assert!(validate_cashu_settings(Some(&cashu_ws), false).is_err());
    }

    #[test]
    fn rejects_zero_locktime_days() {
        // Track A §4B: the seller-recovery locktime floor cannot be zero.
        let cashu = enabled("https://mint.example.com", 0);
        assert!(validate_cashu_settings(Some(&cashu), false).is_err());
    }
}

#[cfg(test)]
mod startup_validation_tests {
    use super::*;
    use crate::config::constants::{
        LND_DEFAULT_HOLD_EXPIRY_DELTA, MAX_DEV_FEE_PERCENTAGE, MIN_DEV_FEE_PERCENTAGE,
        MIN_ESCROW_DEADLINE_MARGIN_BLOCKS,
    };
    use crate::config::types::{
        AntiAbuseBondSettings, CashuSettings, DatabaseSettings, LightningSettings, MostroSettings,
        NostrSettings, RpcSettings,
    };

    fn base_settings() -> Settings {
        Settings {
            database: DatabaseSettings::default(),
            // The shipped CLTV delta: `LightningSettings::default()` leaves it
            // at 0, below any valid escrow deadline margin.
            lightning: LightningSettings {
                hold_invoice_cltv_delta: 144,
                ..Default::default()
            },
            nostr: NostrSettings::default(),
            mostro: MostroSettings::default(),
            rpc: RpcSettings::default(),
            expiration: None,
            anti_abuse_bond: None,
            cashu: None,
            price: None,
            reputation_import: None,
            payer_history: None,
        }
    }

    fn lightning(cltv_delta: u32, margin: u32) -> LightningSettings {
        LightningSettings {
            hold_invoice_cltv_delta: cltv_delta,
            escrow_deadline_margin_blocks: margin,
            ..Default::default()
        }
    }

    fn enabled_cashu() -> CashuSettings {
        CashuSettings {
            enabled: true,
            mint_urls: vec!["https://mint.example.com".to_string()],
            escrow_locktime_days: 15,
        }
    }

    #[test]
    fn escrow_deadline_margin_inside_the_window_is_accepted() {
        for margin in [MIN_ESCROW_DEADLINE_MARGIN_BLOCKS, 24, 143] {
            assert!(
                validate_escrow_deadline_margin(&lightning(144, margin), false).is_ok(),
                "margin {margin} with cltv delta 144 must be accepted"
            );
        }
    }

    #[test]
    fn escrow_deadline_margin_at_or_above_cltv_delta_is_rejected() {
        for margin in [144, 1_000] {
            let err = validate_escrow_deadline_margin(&lightning(144, margin), false)
                .expect_err("margin >= cltv delta must fail");
            assert!(err
                .to_string()
                .contains("must be below hold_invoice_cltv_delta"));
        }
    }

    #[test]
    fn escrow_deadline_margin_below_the_minimum_is_rejected() {
        // 13 clears LND's default holdexpirydelta by one block only, so the
        // guardian would race LND for the escrow.
        for margin in [
            0,
            LND_DEFAULT_HOLD_EXPIRY_DELTA,
            LND_DEFAULT_HOLD_EXPIRY_DELTA + 1,
            MIN_ESCROW_DEADLINE_MARGIN_BLOCKS - 1,
        ] {
            let err = validate_escrow_deadline_margin(&lightning(144, margin), false)
                .expect_err("margin below the minimum must fail");
            assert!(err.to_string().contains(&format!(
                "must be at least {MIN_ESCROW_DEADLINE_MARGIN_BLOCKS}"
            )));
        }
    }

    #[test]
    fn zero_cltv_delta_is_rejected_in_lightning_mode() {
        let mut settings = base_settings();
        settings.lightning = lightning(0, 24);
        let err = validate_mostro_settings(&settings).expect_err("cltv delta 0 must fail");
        assert!(err
            .to_string()
            .contains("must be below hold_invoice_cltv_delta"));
    }

    #[test]
    fn disabled_cashu_section_still_validates_the_lightning_margin() {
        // `[cashu]` present but `enabled = false` is a Lightning node: its
        // margin is checked like any other.
        let mut settings = base_settings();
        settings.lightning = lightning(144, LND_DEFAULT_HOLD_EXPIRY_DELTA);
        settings.cashu = Some(CashuSettings {
            enabled: false,
            ..enabled_cashu()
        });
        let err =
            validate_mostro_settings(&settings).expect_err("lightning margin must be checked");
        assert!(err.to_string().contains("must be at least"));
    }

    #[test]
    fn cashu_mode_skips_escrow_deadline_margin_validation() {
        // A Cashu node has no hold invoice, so its unused `[lightning]`
        // values must not stop it from starting.
        let mut settings = base_settings();
        settings.lightning = LightningSettings::default();
        settings.cashu = Some(enabled_cashu());
        assert!(validate_mostro_settings(&settings).is_ok());
    }

    #[test]
    fn default_settings_pass_validation() {
        assert!(validate_mostro_settings(&base_settings()).is_ok());
    }

    /// Goes through `validate_mostro_settings`, as the daemon does, so
    /// dropping the `[reputation_import]` check from startup is caught.
    #[test]
    fn a_key_listed_under_two_issuers_stops_the_load() {
        use crate::config::types::{ReputationImportSettings, ReputationIssuer};
        let key = nostr_sdk::prelude::Keys::generate().public_key().to_hex();
        let issuer = |name: &str| ReputationIssuer {
            name: name.to_string(),
            keys: vec![key.clone()],
        };
        let mut settings = base_settings();
        settings.reputation_import = Some(ReputationImportSettings {
            enabled: true,
            issuers: vec![issuer("lnp2pbot"), issuer("renamed")],
            ..Default::default()
        });
        let err = validate_mostro_settings(&settings)
            .expect_err("a key under two issuers must stop the load");
        assert!(err.to_string().contains("one issuer only"));
    }

    #[test]
    fn dev_fee_below_minimum_is_rejected() {
        let mut settings = base_settings();
        settings.mostro.dev_fee_percentage = MIN_DEV_FEE_PERCENTAGE - 0.01;
        let err = validate_mostro_settings(&settings).expect_err("below-min dev fee must fail");
        assert!(err.to_string().contains("below minimum"));
    }

    #[test]
    fn dev_fee_above_maximum_is_rejected() {
        let mut settings = base_settings();
        settings.mostro.dev_fee_percentage = MAX_DEV_FEE_PERCENTAGE + 0.01;
        let err = validate_mostro_settings(&settings).expect_err("above-max dev fee must fail");
        assert!(err.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn cashu_and_bond_conflict_is_rejected_through_full_validation() {
        let mut settings = base_settings();
        settings.anti_abuse_bond = Some(AntiAbuseBondSettings {
            enabled: true,
            ..Default::default()
        });
        settings.cashu = Some(enabled_cashu());
        assert!(validate_mostro_settings(&settings).is_err());
    }
}

#[cfg(test)]
mod env_file_tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mostro-config-util-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn missing_env_file_is_a_noop() {
        let dir = temp_dir("no-env");
        // Must not error or panic when `<dir>/.env` is absent.
        load_env_file(&dir);
    }

    #[test]
    fn env_file_values_become_process_env() {
        let dir = temp_dir("with-env");
        // A variable name no other test uses, so parallel runs can't race.
        std::fs::write(
            dir.join(ENV_FILENAME),
            "MOSTRO_TEST_ENV_FILE_MARKER=loaded\n",
        )
        .expect("write .env");
        load_env_file(&dir);
        assert_eq!(
            std::env::var("MOSTRO_TEST_ENV_FILE_MARKER").as_deref(),
            Ok("loaded")
        );
    }

    #[test]
    fn unreadable_env_file_logs_and_continues() {
        let dir = temp_dir("bad-env");
        // A directory named `.env` makes dotenvy fail; the loader must warn
        // and fall back instead of propagating the error.
        std::fs::create_dir_all(dir.join(ENV_FILENAME)).expect("create .env dir");
        load_env_file(&dir);
    }
}

#[cfg(test)]
mod init_configuration_file_tests {
    use super::*;

    fn temp_config_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mostro-init-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    // NOTE: the success path (valid settings.toml) calls
    // `init_mostro_settings`, which panics when the global OnceLock is
    // already set by another test — and the missing-file path calls
    // `std::process::exit(0)` when stdin is not a terminal, which would
    // kill the whole test binary. Only the error paths are testable here.

    #[test]
    fn malformed_toml_is_rejected() {
        let dir = temp_config_dir("bad-toml");
        std::fs::write(dir.join("settings.toml"), "this is not = [valid toml")
            .expect("write settings.toml");
        let result = init_configuration_file(Some(dir.to_string_lossy().into_owned()));
        assert!(result.is_err());
    }

    #[test]
    fn structurally_valid_toml_with_bad_dev_fee_is_rejected() {
        let dir = temp_config_dir("bad-dev-fee");
        // Start from the shipped template so the TOML parses, then push the
        // dev fee out of range so validation (not parsing) rejects it.
        let template = std::str::from_utf8(include_bytes!("../../settings.tpl.toml"))
            .expect("template is UTF-8");
        let tampered =
            template.replace("dev_fee_percentage = ", "dev_fee_percentage = 99.0 # was: ");
        assert!(
            tampered.contains("99.0"),
            "template must contain dev_fee_percentage for this test to be meaningful"
        );
        std::fs::write(dir.join("settings.toml"), tampered).expect("write settings.toml");
        let result = init_configuration_file(Some(dir.to_string_lossy().into_owned()));
        assert!(result.is_err());
    }
}
