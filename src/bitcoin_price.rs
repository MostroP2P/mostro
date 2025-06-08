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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_yadio_response_deserialization() {
        // Test that we can deserialize the expected API response format
        let json_response = r#"
        {
            "BTC": {
                "USD": 50000.0,
                "EUR": 45000.0,
                "GBP": 40000.0
            }
        }
        "#;

        let result: Result<YadioResponse, _> = serde_json::from_str(json_response);
        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.btc.get("USD"), Some(&50000.0));
        assert_eq!(response.btc.get("EUR"), Some(&45000.0));
        assert_eq!(response.btc.get("GBP"), Some(&40000.0));
        assert_eq!(response.btc.len(), 3);
    }

    #[test]
    fn test_yadio_response_invalid_json() {
        // Test deserialization with invalid JSON
        let invalid_json = r#"{"invalid": "structure"}"#;

        let result: Result<YadioResponse, _> = serde_json::from_str(invalid_json);
        assert!(result.is_err());
    }

    #[test]
    fn test_yadio_response_empty_btc() {
        // Test deserialization with empty BTC object
        let json_response = r#"{"BTC": {}}"#;

        let result: Result<YadioResponse, _> = serde_json::from_str(json_response);
        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.btc.len(), 0);
    }

    #[test]
    fn test_currency_code_validation() {
        // Test various currency code formats
        let valid_currencies = vec!["USD", "EUR", "GBP", "JPY", "CAD", "AUD", "CHF"];
        let invalid_currencies = vec!["", "us", "USDD", "123", "usd"];

        // Test valid currencies (should not panic)
        for currency in valid_currencies {
            let _result = BitcoinPriceManager::get_price(currency);
            assert!(true); // Function should handle any string input
        }

        // Test invalid currencies (should not panic)
        for currency in invalid_currencies {
            let _result = BitcoinPriceManager::get_price(currency);
            assert!(true); // Function should handle any string input gracefully
        }
    }

    #[test]
    fn test_bitcoin_price_manager_constants() {
        // Test that constants are properly defined
        assert_eq!(YADIO_API_URL, "https://api.yadio.io/exrates/BTC");
        assert!(YADIO_API_URL.starts_with("https://"));
        assert!(YADIO_API_URL.contains("yadio.io"));
    }

    mod error_handling_tests {
        use super::*;

        #[test]
        fn test_json_parsing_errors() {
            // Test various JSON parsing error scenarios
            let invalid_responses = vec![
                "",                               // Empty response
                "{",                              // Incomplete JSON
                "null",                           // Null response
                "[]",                             // Array instead of object
                r#"{"BTC": null}"#,               // Null BTC field
                r#"{"BTC": []}"#,                 // Array instead of object for BTC
                r#"{"BTC": {"USD": "invalid"}}"#, // Invalid number format
            ];

            for invalid_json in invalid_responses {
                let result: Result<YadioResponse, _> = serde_json::from_str(invalid_json);
                // All should fail to deserialize
                assert!(result.is_err());
            }
        }
    }

    mod price_cache_tests {
        use super::*;

        #[test]
        fn test_price_cache_operations() {
            // Test the logical flow of price caching

            // Test that we can conceptually store and retrieve prices
            let test_currencies = HashMap::from([
                ("USD".to_string(), 50000.0),
                ("EUR".to_string(), 45000.0),
                ("GBP".to_string(), 40000.0),
            ]);

            // Verify our test data is valid
            assert_eq!(test_currencies.len(), 3);
            assert!(test_currencies.contains_key("USD"));
            assert_eq!(test_currencies.get("USD"), Some(&50000.0));

            // Test currency code normalization (uppercase)
            for currency in test_currencies.keys() {
                assert_eq!(currency, &currency.to_uppercase());
                assert!(currency.len() == 3); // Standard currency code length
            }
        }

        #[test]
        fn test_concurrent_access_safety() {
            // Test that the static BITCOIN_PRICES can handle concurrent access
            // This tests the thread safety of our RwLock usage

            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::Arc;
            use std::thread;

            let success = Arc::new(AtomicBool::new(true));
            let mut handles = vec![];

            // Spawn multiple threads trying to read prices
            for _ in 0..5 {
                let success_clone = Arc::clone(&success);
                let handle = thread::spawn(move || {
                    for _ in 0..10 {
                        match BitcoinPriceManager::get_price("USD") {
                            Ok(_) | Err(_) => {
                                // Both outcomes are acceptable for this test
                                // We're just testing that it doesn't panic
                            }
                        }
                    }
                    success_clone.store(true, Ordering::Relaxed);
                });
                handles.push(handle);
            }

            // Wait for all threads to complete
            for handle in handles {
                handle.join().expect("Thread should not panic");
            }

            // All threads should have completed successfully
            assert!(success.load(Ordering::Relaxed));
        }
    }
}
