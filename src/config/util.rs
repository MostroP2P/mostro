/// Utility functions for the config module
/// This module provides utility functions for the config module.
/// It includes functions to initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// It also includes functions to add a trailing slash to a path if it doesn't already have one.
use crate::config::constants::{ENV_FILENAME, MAX_DEV_FEE_PERCENTAGE, MIN_DEV_FEE_PERCENTAGE};
use crate::config::secret::read_nsec_env_var;
use crate::config::wizard;
use crate::config::{init_mostro_settings, Settings};
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
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
/// - When enabled, `mint_url` must be non-empty and parse as `http`/`https`.
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

    let url = reqwest::Url::parse(&cashu.mint_url).map_err(|e| {
        MostroInternalErr(ServiceError::IOError(format!(
            "cashu.mint_url ({:?}) is not a valid URL: {e}",
            cashu.mint_url
        )))
    })?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(MostroInternalErr(ServiceError::IOError(format!(
            "cashu.mint_url must use http or https, got scheme {:?}",
            url.scheme()
        ))));
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
        tracing::info!("Settings correctly loaded!");
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

    tracing::info!("Settings correctly loaded!");

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
mod cashu_validation_tests {
    use super::*;
    use crate::config::types::CashuSettings;

    fn enabled(mint_url: &str, days: u32) -> CashuSettings {
        CashuSettings {
            enabled: true,
            mint_url: mint_url.to_string(),
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
