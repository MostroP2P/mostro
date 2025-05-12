/// Utility functions for the config module
/// This module provides utility functions for the config module.
/// It includes functions to initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// It also includes functions to add a trailing slash to a path if it doesn't already have one.
use crate::config::{init_mostro_settings, Settings};
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use std::fs;
use std::path::PathBuf;

/// Initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// Checks if the directory already exists, and if not, creates it and writes the template file.
/// If a custom config path is provided, it uses that instead of the default `~/.mostro` directory.
pub fn init_configuration_file(config_path: Option<String>) -> Result<(), MostroError> {
    let settings_dir = if let Some(user_path) = config_path {
        PathBuf::from(user_path)
    } else {
        let home_dir = dirs::home_dir()
            .ok_or_else(|| MostroInternalErr(ServiceError::IOError("Could not find home directory".to_string())))?;
        let package_name = env!("CARGO_PKG_NAME");
        home_dir.join(format!(".{}", package_name))
    };

    // Check if /.mostro directory exists
    if !settings_dir.exists() {
        std::fs::create_dir_all(&settings_dir)
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
    }
    let config_file_path = settings_dir.join("settings.toml");
    // Check if settings.toml file exists
    if !config_file_path.exists() {
        std::fs::write(&config_file_path, include_bytes!("../../settings.tpl.toml"))
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

        println!(
            "Created settings file from template at {} for Mostro - Edit it to configure your Mostro instance",
            config_file_path.display()
        );
        std::process::exit(0);
    }
    // Read the file content
    let contents = fs::read_to_string(&config_file_path)
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    // Parse TOML content
    let mut settings: Settings = toml::from_str(&contents)
        .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

    // Override database URL
    settings.database.url = format!("sqlite://{}", settings_dir.join("mostro.db").display());

    // Initialize the global settings variable
    init_mostro_settings(settings);

    tracing::info!("Settings correctly loaded!");

    Ok(())
}
