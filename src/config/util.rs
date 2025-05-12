/// Utility functions for the config module
/// This module provides utility functions for the config module.
/// It includes functions to initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// It also includes functions to add a trailing slash to a path if it doesn't already have one.
use crate::config::Settings;
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use std::fs;
use std::path::{Path, PathBuf};

/// This function creates a Mostro settings template file if it does not exist.
/// User will be prompted to edit the file with the correct settings.
pub fn create_template_file(
    file_name: &str,
    config_path: &Path,
) -> Result<Settings, Box<dyn std::error::Error>> {
    // If the settings file does not exist, create it from the template
    if !Path::new(file_name).exists() {
        println!("Settings file not found - creating default settings file");
        std::fs::write(file_name, include_bytes!("../../settings.tpl.toml"))
            .expect("Failed to write template file");
        // Print a message to the user
        println!(
            "Created settings file from template at {} for Mostro",
            config_path.display()
        );
        println!("Please edit the settings file with  your settings and run Mostro again");
        std::process::exit(0);
    }
    // If the settings file exists, read it and return the settings
    else {
        println!("Settings file found at {}", config_path.display());
        let config_file_path = config_path.join("settings.toml");
        // Read the file content
        let contents = fs::read_to_string(&config_file_path)?;

        // Parse TOML content
        let mut settings: Settings = toml::from_str(&contents)?;

        // Override database URL
        settings.database.url = format!("sqlite://{}", config_path.join("mostro.db").display());
        Ok(settings)
    }
}

/// Initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// Checks if the directory already exists, and if not, creates it and writes the template file.
/// If a custom config path is provided, it uses that instead of the default `~/.mostro` directory.
pub fn init_default_dir(config_path: Option<String>) -> Result<PathBuf, MostroError> {
    let settings_dir = if let Some(path) = config_path {
        PathBuf::from(path)
    } else {
        let home = std::env::var("HOME")
            .map_err(|e| MostroInternalErr(ServiceError::EnvVarError(e.to_string())))?;
        let package_name = std::env::var("CARGO_PKG_NAME")
            .unwrap_or_else(|_| ".mostro".to_string());
        PathBuf::from(home).join(package_name)
    };

    if !settings_dir.exists() {
        std::fs::create_dir_all(&settings_dir)
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

        let config_path = settings_dir.join("settings.toml");
        std::fs::write(&config_path, include_bytes!("../../settings.tpl.toml"))
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

        println!(
            "Created settings file from template at {} for mostro - Edit it to configure your Mostro instance",
            config_path.display()
        );
        std::process::exit(0);
    }

    Ok(settings_dir)
}
