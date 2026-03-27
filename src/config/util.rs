/// Utility functions for the config module
/// This module provides utility functions for the config module.
/// It includes functions to initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// It also includes functions to add a trailing slash to a path if it doesn't already have one.
use crate::config::constants::{MAX_DEV_FEE_PERCENTAGE, MIN_DEV_FEE_PERCENTAGE};
use crate::config::wizard;
use crate::config::{init_mostro_settings, Settings};
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;

const DB_FILENAME: &str = "mostro.db";

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

    if settings.nostr.nsec_privkey.is_some() {
        return Err(MostroInternalErr(ServiceError::IOError(
            "nostr.nsec_privkey is no longer supported; move the key to nostr.nsec_privkey_file"
                .to_string(),
        )));
    }

    if settings.nostr.nsec_privkey_file.trim().is_empty() {
        return Err(MostroInternalErr(ServiceError::IOError(
            "Missing Nostr private key file configuration".to_string(),
        )));
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
    let config_file_path = settings_dir.join("settings.toml");

    if !config_file_path.exists() {
        let settings = if std::io::stdin().is_terminal() {
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

        validate_mostro_settings(&settings)?;
        init_mostro_settings(settings);
        tracing::info!("Settings correctly loaded!");
        return Ok(());
    }

    // Read the file content
    let contents = fs::read_to_string(&config_file_path)
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    // Parse TOML content
    let mut settings: Settings = toml::from_str(&contents)
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    // Validate settings before initializing
    validate_mostro_settings(&settings)?;

    // Override database URL
    settings.database.url = format!("sqlite://{}", settings_dir.join(DB_FILENAME).display());

    // Initialize the global settings variable
    init_mostro_settings(settings);

    tracing::info!("Settings correctly loaded!");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{
        DatabaseSettings, LightningSettings, MostroSettings, NostrSettings, RpcSettings,
    };

    fn make_settings(nostr: NostrSettings) -> Settings {
        Settings {
            database: DatabaseSettings::default(),
            lightning: LightningSettings::default(),
            nostr,
            mostro: MostroSettings::default(),
            rpc: RpcSettings::default(),
            expiration: None,
        }
    }

    #[test]
    fn validate_mostro_settings_rejects_legacy_inline_nsec() {
        let settings = make_settings(NostrSettings {
            nsec_privkey: Some(
                "nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd".to_string(),
            ),
            relays: vec!["wss://relay.test".to_string()],
            ..Default::default()
        });

        let error = validate_mostro_settings(&settings).expect_err("inline nsec must be rejected");
        assert!(error
            .to_string()
            .contains("nostr.nsec_privkey is no longer supported"));
    }

    #[test]
    fn validate_mostro_settings_requires_private_key_file() {
        let settings = make_settings(NostrSettings {
            relays: vec!["wss://relay.test".to_string()],
            ..Default::default()
        });

        let error =
            validate_mostro_settings(&settings).expect_err("missing key file must be rejected");
        assert!(error
            .to_string()
            .contains("Missing Nostr private key file configuration"));
    }
}
