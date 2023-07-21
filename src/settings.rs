use anyhow::Error;
use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;

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
        let s = Config::builder()
            .add_source(File::with_name("settings"))
            // Add in settings from the environment (with a prefix of APP)
            // Eg.. `APP_DEBUG=1 ./target/app` would set the `debug` key
            .add_source(Environment::with_prefix("app"))
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
