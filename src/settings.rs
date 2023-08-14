use crate::MOSTRO_CONFIG;
use anyhow::{Error, Result};
use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{stdin, stdout, BufRead, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process;

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Database {
    pub url: String,
}

impl TryFrom<Settings> for Database {
    type Error = Error;

    fn try_from(_: Settings) -> Result<Self, Error> {
        Ok(MOSTRO_CONFIG.lock().unwrap().database.clone())
    }
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Lightning {
    pub lnd_cert_file: String,
    pub lnd_macaroon_file: String,
    pub lnd_grpc_host: String,
    pub lnd_grpc_port: u32,
    pub invoice_expiration_window: u32,
    pub hold_invoice_cltv_delta: u32,
    pub hold_invoice_expiration_window: u32,
}

impl TryFrom<Settings> for Lightning {
    type Error = Error;

    fn try_from(_: Settings) -> Result<Self, Error> {
        Ok(MOSTRO_CONFIG.lock().unwrap().lightning.clone())
    }
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Nostr {
    pub nsec_privkey: String,
    pub relays: Vec<String>,
}

impl TryFrom<Settings> for Nostr {
    type Error = Error;

    fn try_from(_: Settings) -> Result<Self, Error> {
        Ok(MOSTRO_CONFIG.lock().unwrap().nostr.clone())
    }
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Mostro {
    pub fee: f64,
    pub max_routing_fee: f64,
    pub max_order_amount: u32,
    pub min_payment_amount: u32,
    pub expiration_hours: u32,
    pub expiration_seconds: u32,
}

impl TryFrom<Settings> for Mostro {
    type Error = Error;

    fn try_from(_: Settings) -> Result<Self, Error> {
        Ok(MOSTRO_CONFIG.lock().unwrap().mostro.clone())
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub database: Database,
    pub nostr: Nostr,
    pub mostro: Mostro,
    pub lightning: Lightning,
}

pub fn init_global_settings(setting: Settings) {
    *MOSTRO_CONFIG.lock().unwrap() = setting
}

impl Settings {
    pub fn new(mut config_path: PathBuf) -> Result<Self, ConfigError> {
        let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "dev".into());
        let file_name = {
            if config_path.as_os_str().as_bytes().last() != Some(&b'/') {
                let tmp = format!("settings.{}.toml", run_mode);
                config_path.push(tmp);
                format!("{}", config_path.display())
            } else {
                format!("{}settings.{}.toml", config_path.display(), run_mode)
            }
        };

        let s = Config::builder()
            .add_source(File::with_name(&file_name).required(true))
            // Add in settings from the environment (with a prefix of APP)
            // Eg.. `APP_DEBUG=1 ./target/app` would set the `debug` key
            .add_source(Environment::with_prefix("app"))
            .build()?;

        // You can deserialize the entire configuration as
        s.try_deserialize()
    }

    pub fn get_ln() -> Lightning {
        MOSTRO_CONFIG.lock().unwrap().lightning.clone()
    }

    pub fn get_mostro() -> Mostro {
        MOSTRO_CONFIG.lock().unwrap().mostro.clone()
    }

    pub fn get_db() -> Database {
        MOSTRO_CONFIG.lock().unwrap().database.clone()
    }

    pub fn get_nostr() -> Nostr {
        MOSTRO_CONFIG.lock().unwrap().nostr.clone()
    }
}

pub fn init_default_dir(config_path: Option<&String>) -> Result<PathBuf> {
    // , final_path : &mut PathBuf) -> Result<()> {
    // Dir prefix
    let home_dir: OsString;
    // Complete path to file variable
    let mut settings_dir_default = std::path::PathBuf::new();

    if let Some(path) = config_path {
        // Os String
        home_dir = path.to_string().into();
        // Create default path from custom path
        settings_dir_default.push(home_dir);
    } else {
        // Get $HOME from env
        let tmp = std::env::var("HOME").unwrap();
        // Os String
        home_dir = tmp.into();
        // Create default path with default .mostro value
        settings_dir_default.push(home_dir);
        settings_dir_default.push(".mostro");
    }

    // Check if default folder exists
    let folder_default = settings_dir_default.is_dir();

    // If settings dir is not existing
    if !folder_default {
        println!(
            "Creating .mostro default settings dir {:?}",
            settings_dir_default
        );
        print!("Are you sure? (Y/n) > ");

        // Ask user confirm for default folder
        let mut user_input = String::new();
        let _input = stdin();

        stdout().flush()?;

        let mut answer = stdin().lock();
        answer.read_line(&mut user_input)?;

        match user_input.to_lowercase().as_str().trim_end() {
            "y" | "" => {
                fs::create_dir(settings_dir_default.clone())?;
                println!("Ok! You have created the folder for settings file");
                println!("Copy the settings.toml file with template in {:?} folder than edit field with correct values",settings_dir_default);
                process::exit(0);
            }
            "n" => {
                println!("Ok try again with another folder...");
                process::exit(0);
            }
            &_ => {
                println!("Can't get what you're sayin!");
                process::exit(0);
            }
        };
    } else {
        // Set path
        Ok(settings_dir_default)
    }
}
