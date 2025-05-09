use crate::config::types::{Database, Lightning, Mostro, Nostr};
use crate::config::util::{add_trailing_slash, has_trailing_slash};
use crate::MOSTRO_CONFIG;
use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;
use std::path::{Path, PathBuf};

// / Mostro configuration settings struct
#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub database: Database,
    pub nostr: Nostr,
    pub mostro: Mostro,
    pub lightning: Lightning,
}

/// Initialize the global MOSTRO_CONFIG struct
pub fn init_global_settings(s: Settings) {
    MOSTRO_CONFIG.set(s).unwrap()
}
/// This function initializes the global MOSTRO_CONFIG struct with the provided Settings instance.
impl Settings {
    pub fn new(mut config_path: PathBuf) -> Result<Self, ConfigError> {
        use std::env;
        let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "".into());
        let run_mode = format!("{}.toml", run_mode);
        let file_name = {
            if !has_trailing_slash(config_path.as_path()) {
                add_trailing_slash(&mut config_path);
                let tmp = format!("{}settings{}", config_path.display(), run_mode);
                tmp
            } else {
                format!("{}settings{}", config_path.display(), run_mode)
            }
        };
        // If the settings file does not exist but the directory exists, create it from the template
        if !Path::new(&file_name).exists() {
            println!("Settings file not found - creating default settings file");
            std::fs::write(&file_name, include_bytes!("../../settings.tpl.toml"))
                .map_err(|e| ConfigError::Message(e.to_string()))?;
            // Print a message to the user
            println!(
                "Created settings file from template at {} for mostro - Edit it to configure your Mostro instance",
                config_path.display()
            );
            std::process::exit(0);
        }

        let s = Config::builder()
            .add_source(File::with_name(&file_name).required(true))
            // Add in settings from the environment (with a prefix of APP)
            // Eg.. `APP_DEBUG=1 ./target/app` would set the `debug` key
            .add_source(Environment::with_prefix("app"))
            .set_override(
                "database.url",
                format!("sqlite://{}", config_path.display()),
            )?
            .build()?;

        // You can deserialize the entire configuration as
        s.try_deserialize()
    }

    /// This function retrieves the Lightning configuration from the global MOSTRO_CONFIG struct.
    pub fn get_ln() -> Lightning {
        MOSTRO_CONFIG.get().unwrap().lightning.clone()
    }

    /// This function retrieves the Mostro configuration from the global MOSTRO_CONFIG struct.
    pub fn get_mostro() -> Mostro {
        MOSTRO_CONFIG.get().unwrap().mostro.clone()
    }

    /// This function retrieves the Database configuration from the global MOSTRO_CONFIG struct.
    pub fn get_db() -> Database {
        MOSTRO_CONFIG.get().unwrap().database.clone()
    }

    /// This function retrieves the Nostr configuration from the global MOSTRO_CONFIG struct.
    pub fn get_nostr() -> Nostr {
        MOSTRO_CONFIG.get().unwrap().nostr.clone()
    }
}
