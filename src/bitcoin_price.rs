use mostro_core::prelude::*;
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;
use tracing::info;

const YADIO_API_URL: &str = "https://api.yadio.io/exrates/BTC";

#[derive(Debug, Deserialize)]
struct YadioResponse {
    #[serde(rename = "BTC")]
    btc: HashMap<String, f64>,
}

static BITCOIN_PRICES: Lazy<RwLock<HashMap<String, f64>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

pub struct BitcoinPriceManager;

impl BitcoinPriceManager {
    pub async fn update_prices() -> Result<(), MostroError> {
        let response = reqwest::get(YADIO_API_URL)
            .await
            .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?;
        let yadio_response: YadioResponse = response
            .json()
            .await
            .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;
        info!(
            "Bitcoin prices updated. Got BTC price in {} fiat currencies",
            yadio_response.btc.keys().collect::<Vec<&String>>().len()
        );

        let mut prices_write = BITCOIN_PRICES
            .write()
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
        *prices_write = yadio_response.btc;
        Ok(())
    }

    pub fn get_price(currency: &str) -> Result<f64, MostroError> {
        let prices_read: std::sync::RwLockReadGuard<'_, HashMap<String, f64>> = BITCOIN_PRICES
            .read()
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
        prices_read
            .get(currency)
            .cloned()
            .ok_or(MostroInternalErr(ServiceError::NoAPIResponse))
    }
}
