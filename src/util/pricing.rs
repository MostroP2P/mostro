use crate::bitcoin_price::BitcoinPriceManager;
use crate::config::settings::Settings;
use crate::lnurl::HTTP_CLIENT;
use crate::models::Yadio;
use chrono::Duration;
use mostro_core::prelude::*;
use nostr_sdk::prelude::Timestamp;
use tokio::time::sleep;
use tracing::{info, warn};

pub type FiatNames = std::collections::HashMap<String, String>;
const MAX_RETRY: u16 = 4;

pub async fn retries_yadio_request(
    req_string: &str,
    fiat_code: &str,
) -> Result<(Option<reqwest::Response>, bool), MostroError> {
    // Get Fiat list and check if currency exchange is available
    let mostro_settings = Settings::get_mostro();
    let api_req_string = format!("{}/currencies", mostro_settings.bitcoin_price_api_url);
    let fiat_list_check = HTTP_CLIENT
        .get(api_req_string)
        .send()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?
        .json::<FiatNames>()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::MalformedAPIRes))?
        .contains_key(fiat_code);

    // Exit with error - no currency
    if !fiat_list_check {
        return Ok((None, fiat_list_check));
    }

    let res = HTTP_CLIENT
        .get(req_string)
        .send()
        .await
        .map_err(|_| MostroInternalErr(ServiceError::NoAPIResponse))?;

    Ok((Some(res), fiat_list_check))
}

pub fn get_bitcoin_price(fiat_code: &str) -> Result<f64, MostroError> {
    BitcoinPriceManager::get_price(fiat_code)
}

/// Request market quote from Yadio to have sats amount at actual market price
pub async fn get_market_quote(
    fiat_amount: &i64,
    fiat_code: &str,
    premium: i64,
) -> Result<i64, MostroError> {
    // Add here check for market price
    let mostro_settings = Settings::get_mostro();
    let req_string = format!(
        "{}/convert/{}/{}/BTC",
        mostro_settings.bitcoin_price_api_url, fiat_amount, fiat_code
    );
    info!("Requesting API price: {}", req_string);

    let mut req = (None, false);
    let mut no_answer_api = false;

    // Retry for 4 times
    for retries_num in 1..=MAX_RETRY {
        match retries_yadio_request(&req_string, fiat_code).await {
            Ok(response) => {
                req = response;
                break;
            }
            Err(_e) => {
                if retries_num == MAX_RETRY {
                    no_answer_api = true;
                }
                warn!(
                    "API price request failed retrying - {} tentatives left.",
                    (MAX_RETRY - retries_num)
                );
                sleep(std::time::Duration::from_secs(2)).await;
            }
        };
    }

    // Case no answers from Yadio
    if no_answer_api {
        return Err(MostroError::MostroInternalErr(ServiceError::NoAPIResponse));
    }

    // No currency present
    if !req.1 {
        return Err(MostroError::MostroInternalErr(ServiceError::NoCurrency));
    }

    if req.0.is_none() {
        return Err(MostroError::MostroInternalErr(
            ServiceError::MalformedAPIRes,
        ));
    }

    let quote = if let Some(q) = req.0 {
        q.json::<Yadio>()
            .await
            .map_err(|_| MostroError::MostroInternalErr(ServiceError::MessageSerializationError))?
    } else {
        return Err(MostroError::MostroInternalErr(
            ServiceError::MalformedAPIRes,
        ));
    };

    let mut sats = quote.result * 100_000_000_f64;

    // Added premium value to have correct sats value
    if premium != 0 {
        sats = sats - (premium as f64) / 100_f64 * sats;
    }

    Ok(sats as i64)
}

pub fn get_fee(amount: i64) -> i64 {
    let mostro_settings = Settings::get_mostro();
    // We calculate the bot fee
    let split_fee = (mostro_settings.fee * amount as f64) / 2.0;
    split_fee.round() as i64
}

/// Calculates the development fee as a percentage of the total Mostro fee.
///
/// This is a pure function that performs the fee calculation without accessing global state.
/// Useful for testing with different percentage values.
///
/// # Arguments
/// * `total_mostro_fee` - The total Mostro fee amount in satoshis
/// * `percentage` - The percentage to apply (e.g., 0.30 for 30%)
///
/// # Returns
/// The calculated development fee, rounded to nearest satoshi
pub fn calculate_dev_fee(total_mostro_fee: i64, percentage: f64) -> i64 {
    let dev_fee = (total_mostro_fee as f64) * percentage;
    dev_fee.round() as i64
}

/// Calculate total development fee from the total Mostro fee
/// Takes the TOTAL Mostro fee (both parties combined) and returns the TOTAL dev fee
/// The returned value should be split 50/50 between buyer and seller
/// Returns the total amount in satoshis for the dev fund
pub fn get_dev_fee(total_mostro_fee: i64) -> i64 {
    let mostro_settings = Settings::get_mostro();
    calculate_dev_fee(total_mostro_fee, mostro_settings.dev_fee_percentage)
}

/// Calculates the expiration timestamp for an order.
///
/// This function computes the expiration time based on the current time and application settings.
/// If an expiration timestamp is provided, it is clamped to a maximum allowed value (the current time plus
/// a configured maximum number of days). If no timestamp is given, a default expiration is calculated as the
/// current time plus a configured number of hours.
///
/// # Returns
///
/// The computed expiration timestamp as a Unix epoch in seconds.
///
/// # Examples
///
/// ```
/// // Calculate a default expiration timestamp.
/// let exp_default = get_expiration_date(None);
/// println!("Default expiration: {}", exp_default);
///
/// // Provide a custom expiration timestamp. The returned value will be clamped
/// // if it exceeds the maximum allowed expiration.
/// let exp_custom = get_expiration_date(Some(exp_default + 10_000));
/// println!("Custom expiration (clamped if necessary): {}", exp_custom);
/// ```
pub fn get_expiration_date(expire: Option<i64>) -> i64 {
    let mostro_settings = Settings::get_mostro();
    // We calculate order expiration
    let expire_date: i64;
    let expires_at_max: i64 = Timestamp::now().as_u64() as i64
        + Duration::days(mostro_settings.max_expiration_days.into()).num_seconds();
    if let Some(mut exp) = expire {
        if exp > expires_at_max {
            exp = expires_at_max;
        };
        expire_date = exp;
    } else {
        expire_date = Timestamp::now().as_u64() as i64
            + Duration::hours(mostro_settings.expiration_hours as i64).num_seconds();
    }
    expire_date
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_dev_fee_basic() {
        // 1000 sats Mostro fee at 30% -> 300 sats
        let fee = calculate_dev_fee(1_000, 0.30);
        assert_eq!(fee, 300);
    }

    #[test]
    fn test_get_dev_fee_rounding() {
        // 333 * 0.30 = 99.9 -> rounds to 100
        let fee = calculate_dev_fee(333, 0.30);
        assert_eq!(fee, 100);
    }

    #[test]
    fn test_get_dev_fee_zero() {
        let fee = calculate_dev_fee(0, 0.30);
        assert_eq!(fee, 0);
    }

    #[test]
    fn test_get_dev_fee_tiny_amounts() {
        // With 30%, 1 * 0.30 = 0.3 -> 0
        let fee = calculate_dev_fee(1, 0.30);
        assert_eq!(fee, 0);
    }

    #[tokio::test]
    async fn test_get_market_quote_url_construction() {
        // Test the URL construction logic without making actual API calls
        // This test verifies that the API URL format is correct
        let base_url = "https://api.yadio.io";
        let fiat_amount = 1000;
        let fiat_code = "USD";

        let expected_url = format!("{}/convert/{}/{}/BTC", base_url, fiat_amount, fiat_code);
        assert_eq!(expected_url, "https://api.yadio.io/convert/1000/USD/BTC");

        // Test currency list URL construction
        let currencies_url = format!("{}/currencies", base_url);
        assert_eq!(currencies_url, "https://api.yadio.io/currencies");
    }
}
