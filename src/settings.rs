use anyhow::{Error, Result};
use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{stdin, stdout, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process;

lazy_static! {
    static ref CONFIG_PATH: PathBuf = PathBuf::new();
}

#[derive(Debug, Deserialize)]
pub struct Database {
    pub url: String,
}

impl TryFrom<Settings> for Database {
    type Error = Error;

    fn try_from(_: Settings) -> Result<Self, Error> {
        let db_settings = Settings::new()?;

        Ok(db_settings.database)
    }
}

#[derive(Debug, Deserialize)]
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
        let ln_settings = Settings::new()?;

        Ok(ln_settings.lightning)
    }
}

#[derive(Debug, Deserialize)]
pub struct Nostr {
    pub nsec_privkey: String,
    pub relays: Vec<String>,
}

impl TryFrom<Settings> for Nostr {
    type Error = Error;

    fn try_from(_: Settings) -> Result<Self, Error> {
        let nostr_settings = Settings::new()?;

        Ok(nostr_settings.nostr)
    }
}

#[derive(Debug, Deserialize)]
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
        let mostro_settings = Settings::new()?;

        Ok(mostro_settings.mostro)
    }
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub database: Database,
    pub nostr: Nostr,
    pub mostro: Mostro,
    pub lightning: Lightning,
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "dev".into());
        let file_name = format!("settings.{}.toml", run_mode);
        let s = Config::builder()
            .add_source(File::with_name(&file_name).required(true))
            // Add in settings from the environment (with a prefix of APP)
            // Eg.. `APP_DEBUG=1 ./target/app` would set the `debug` key
            .add_source(Environment::with_prefix("app"))
            .build()?;

        // You can deserialize the entire configuration as
        s.try_deserialize()
    }

    pub fn set_val() -> Result<Self, ConfigError> {
        // let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "dev".into());
        let file_name = format!("settings.toml");
        let s = Config::builder()
            .add_source(File::with_name(&file_name).required(true))
            // Add in settings from the environment (with a prefix of APP)
            // Eg.. `APP_DEBUG=1 ./target/app` would set the `debug` key
            .set_override("database.config_path", "/.mostro")?
            .build()?;

        // You can deserialize the entire configuration as
        s.try_deserialize()
    }

    pub fn get_ln() -> Result<Lightning, Error> {
        let settings = Settings::new()?;

        Ok(settings.lightning)
    }

    pub fn get_mostro() -> Result<Mostro, Error> {
        let settings = Settings::new()?;

        Ok(settings.mostro)
    }

    pub fn get_db() -> Result<Database, Error> {
        let settings = Settings::new()?;

        Ok(settings.database)
    }

    pub fn get_nostr() -> Result<Nostr, Error> {
        let settings = Settings::new()?;

        Ok(settings.nostr)
    }
}

pub fn init_default_dir(config_path: Option<&String>, final_path : &mut String) -> Result<()> {
    // Dir prefix
    let home_dir;
    // Complete path to file variable
    let mut settings_dir_default = std::path::PathBuf::new();

    if config_path.is_none() {
        // Get $HOME from env
        home_dir = std::env::var("HOME").unwrap();
        // Create default path with default .mostro value
        settings_dir_default.push(home_dir);
        settings_dir_default.push(".mostro");
    } else {
        home_dir = config_path.unwrap().to_string();
        // Create default path from custom path
        settings_dir_default.push(home_dir);
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
                println!("Ok you have created the folder for settings file");
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
        // Get kind of config file from .env var
        let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "dev".into());
        let file_name = format!("settings.{}.toml", run_mode);
        settings_dir_default.push(file_name);
        // Check file existence
        if settings_dir_default.exists() {
            Ok(settings_dir_default)
        } else {
            println!("Settings file is not present in the requested path {:?} check file and copy it inside folder",settings_dir_default);
            std::process::exit(0)
        }
    }
}
