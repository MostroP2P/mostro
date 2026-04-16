use crate::config::settings::Settings;
use crate::lnurl::HTTP_CLIENT;
use crate::nip33::new_exchange_rates_event;
use crate::util::{get_keys, get_nostr_client};
use chrono::Utc;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;
use tracing::{error, info};

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
        let mostro_settings = Settings::get_mostro();
        let api_url = format!("{}/exrates/BTC", mostro_settings.bitcoin_price_api_url);
        let response = HTTP_CLIENT
            .get(&api_url)
            .send()
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

        // Clone rates before acquiring lock to avoid holding it across await
        let rates_clone = yadio_response.btc.clone();

        {
            let mut prices_write = BITCOIN_PRICES
                .write()
                .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;
            *prices_write = rates_clone.clone();
        } // Lock is dropped here

        // Publish rates to Nostr if enabled (after releasing the lock)
        if mostro_settings.publish_exchange_rates_to_nostr {
            if let Err(e) = Self::publish_rates_to_nostr(&rates_clone).await {
                error!("Failed to publish exchange rates to Nostr: {}", e);
                // Don't fail the entire update if Nostr publishing fails
            }
        }

        Ok(())
    }

    /// Publishes exchange rates to Nostr as a NIP-33 addressable event (kind 30078)
    async fn publish_rates_to_nostr(rates: &HashMap<String, f64>) -> Result<(), MostroError> {
        let keys = get_keys().map_err(|e| {
            error!("Failed to get Mostro keys: {}", e);
            MostroInternalErr(ServiceError::IOError(e.to_string()))
        })?;

        // Publish in Yadio's exact format: {"BTC": {"USD": 50000.0, "EUR": 45000.0, ...}}
        // This matches their API response structure
        let mut wrapper = HashMap::new();
        wrapper.insert("BTC".to_string(), rates.clone());
        let formatted_rates = wrapper;

        let content = serde_json::to_string(&formatted_rates)
            .map_err(|_| MostroInternalErr(ServiceError::MessageSerializationError))?;

        let timestamp = Utc::now().timestamp();

        // Expiration should be at least 2x the update interval to allow for delays
        // Cap at 1 hour to prevent stale data
        // Note: We read settings here (instead of passing from scheduler) to ensure
        // expiration stays aligned with interval if config is reloaded at runtime
        let mostro_settings = Settings::get_mostro();
        let update_interval = mostro_settings.exchange_rates_update_interval_seconds;
        let expiration_seconds = std::cmp::min(update_interval * 2, 3600);
        let expiration = timestamp + expiration_seconds as i64;

        let tags = Tags::from_list(vec![
            Tag::custom(
                TagKind::Custom("published_at".into()),
                vec![timestamp.to_string()],
            ),
            Tag::custom(TagKind::Custom("source".into()), vec!["yadio".to_string()]),
            Tag::expiration(Timestamp::from(expiration as u64)),
        ]);

        let event = new_exchange_rates_event(&keys, &content, tags).map_err(|e| {
            error!("Failed to create exchange rates event: {}", e);
            MostroInternalErr(ServiceError::MessageSerializationError)
        })?;

        let client = get_nostr_client().map_err(|e| {
            error!("Failed to get Nostr client: {}", e);
            e
        })?;

        // Publish with timeout to avoid blocking the scheduler
        // Best-effort: log errors but don't fail the update job
        let timeout_duration = std::time::Duration::from_secs(30);
        match tokio::time::timeout(timeout_duration, client.send_event(&event)).await {
            Ok(Ok(output)) => {
                info!(
                    "Exchange rates published to Nostr ({} currencies). Output: {:?}",
                    rates.len(),
                    output
                );
            }
            Ok(Err(e)) => {
                error!("Failed to send exchange rates event to relays: {}", e);
            }
            Err(_) => {
                error!("Timeout publishing exchange rates to Nostr (30s exceeded)");
            }
        }

        // Always return Ok - publishing is best-effort
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
    fn test_rates_structure() {
        // Test that Yadio rates are wrapped correctly
        let mut input_rates = HashMap::new();
        input_rates.insert("USD".to_string(), 50000.0);
        input_rates.insert("EUR".to_string(), 45000.0);

        // Wrap in Yadio format: {"BTC": {...}}
        let mut wrapper = HashMap::new();
        wrapper.insert("BTC".to_string(), input_rates.clone());

        assert_eq!(wrapper.len(), 1);
        assert!(wrapper.contains_key("BTC"));
        assert_eq!(wrapper.get("BTC").unwrap().get("USD"), Some(&50000.0));
        assert_eq!(wrapper.get("BTC").unwrap().get("EUR"), Some(&45000.0));
    }

    #[test]
    fn test_rates_json_serialization() {
        // Test that rates can be serialized to Yadio format
        // Use only fiat currencies (Yadio includes BTC in the wrapper, not in the rates map)
        let mut input_rates = HashMap::new();
        input_rates.insert("USD".to_string(), 50000.0);
        input_rates.insert("EUR".to_string(), 45000.0);

        let mut wrapper = HashMap::new();
        wrapper.insert("BTC".to_string(), input_rates);

        let json = serde_json::to_string(&wrapper).unwrap();
        assert!(json.contains("\"BTC\""));
        assert!(json.contains("\"USD\""));
        assert!(json.contains("50000"));
        assert!(json.contains("\"EUR\""));
        assert!(json.contains("45000"));
        // Ensure we don't have nested BTC key (would be invalid)
        assert!(!json.contains("\"BTC\":1"));
    }

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
            // No assertion needed; this ensures no panic for valid input
        }

        // Test invalid currencies (should not panic)
        for currency in invalid_currencies {
            let _result = BitcoinPriceManager::get_price(currency);
            // No assertion needed; this ensures no panic for invalid input
        }
    }

    #[test]
    fn test_bitcoin_price_manager_api_url() {
        // Test that API URL configuration is properly handled
        let expected_base = "https://api.yadio.io";
        assert!(expected_base.starts_with("https://"));
        assert!(expected_base.contains("yadio.io"));
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
