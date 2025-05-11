use crate::config::types::{DatabaseSettings, LightningSettings, MostroSettings, NostrSettings};
use crate::MOSTRO_CONFIG;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

// Mostro configuration settings struct
#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub database: DatabaseSettings,
    pub nostr: NostrSettings,
    pub mostro: MostroSettings,
    pub lightning: LightningSettings,
}

/// Initialize the global MOSTRO_CONFIG struct
pub fn init_global_settings(s: Settings) {
    MOSTRO_CONFIG.set(s).unwrap()
}

/// This function creates a Mostro settings template file if it does not exist.
/// User will be prompted to edit the file with the correct settings.
fn create_template_file(
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

impl Settings {
    pub fn new(config_path: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        // Get the file name from the config path
        let file_name = config_path.display().to_string();

        // If the settings file does not exist but the directory exists, create it from the template
        let settings = create_template_file(&file_name, &config_path)?;

        Ok(settings)
    }

    /// This function retrieves the Lightning configuration from the global MOSTRO_CONFIG struct.
    pub fn get_ln() -> &'static LightningSettings {
        &MOSTRO_CONFIG.get().unwrap().lightning
    }

    /// This function retrieves the Mostro configuration from the global MOSTRO_CONFIG struct.
    pub fn get_mostro() -> &'static MostroSettings {
        &MOSTRO_CONFIG.get().unwrap().mostro
    }

    /// This function retrieves the Database configuration from the global MOSTRO_CONFIG struct.
    pub fn get_db() -> &'static DatabaseSettings {
        &MOSTRO_CONFIG.get().unwrap().database
    }

    /// This function retrieves the Nostr configuration from the global MOSTRO_CONFIG struct.
    pub fn get_nostr() -> &'static NostrSettings {
        &MOSTRO_CONFIG.get().unwrap().nostr
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::settings_init;

    use super::*;

    #[test]
    fn test_create_template_file() {
        let mut home_dir = std::env::var("HOME").unwrap();
        home_dir.push_str("/.mostro");
        let home_dir = Path::new(&home_dir);
        let template_file = "settings.tpl.toml";
        let settings = create_template_file(&template_file, &home_dir).unwrap();
        assert_eq!(settings.database.url, "sqlite://test_config.toml");
    }
}
