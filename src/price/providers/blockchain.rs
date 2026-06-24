//! Blockchain.com direct BTC quoter (spec §11.4).
//!
//! Calls `GET {url}/ticker` and maps the
//! `{ "USD": { "15m": …, "last": …, "buy": …, "sell": …, "symbol": "USD" } }`
//! body into per-currency [`Quote::PerBtc`] entries. The adapter takes
//! **`last`** (mid-market) and discards `buy`/`sell` — Mostro prices at
//! mid-market and never bakes in an exchange spread (§6.6, §11.6); the
//! order premium/fee is the only markup, applied downstream.
//!
//! Keyless, ~28 major fiats, no CUP/MLC — a redundancy anchor for
//! USD/EUR/GBP/JPY, not a long-tail source.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;

use crate::price::config::ProviderConfig;
use crate::price::provider::{PriceProvider, ProviderError, ProviderId, ProviderQuotes, Quote};

/// One ticker entry. Only `last` (mid-market) is used; the bid/ask fields
/// are intentionally not even deserialised so a future refactor cannot
/// accidentally reach for them (§6.6 mid-market rule).
#[derive(Debug, Deserialize)]
struct TickerEntry {
    last: Option<f64>,
}

/// Direct BTC quoter against the Blockchain.com ticker.
#[derive(Debug)]
pub struct BlockchainProvider {
    url: String,
}

impl BlockchainProvider {
    /// Build the provider from its `[price.providers.blockchain]` sub-table.
    pub fn new(cfg: &ProviderConfig) -> Self {
        Self {
            url: cfg.url.trim_end_matches('/').to_string(),
        }
    }

    /// Parse a `/ticker` payload into [`ProviderQuotes`]. Split out from
    /// [`PriceProvider::fetch`] so it is testable against the captured
    /// fixture without HTTP (spec §10.5).
    pub(crate) fn parse(body: &str) -> Result<ProviderQuotes, ProviderError> {
        let parsed: HashMap<String, TickerEntry> = serde_json::from_str(body)
            .map_err(|e| ProviderError::Parse(format!("blockchain: {e}")))?;
        Ok(parsed
            .into_iter()
            .filter_map(|(code, entry)| match entry.last {
                Some(v) if v.is_finite() && v > 0.0 => {
                    // Codes arrive uppercase already; canonicalise anyway so
                    // the adapter honours §6.6 even if the API drifts.
                    Some((code.to_uppercase(), Quote::PerBtc(v)))
                }
                _ => None,
            })
            .collect())
    }
}

#[async_trait]
impl PriceProvider for BlockchainProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Blockchain
    }

    async fn fetch(&self, http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
        let url = format!("{}/ticker", self.url);
        let res = http
            .get(&url)
            .send()
            .await
            .map_err(|e| ProviderError::Http(format!("blockchain GET {url}: {e}")))?;
        if !res.status().is_success() {
            return Err(ProviderError::Http(format!(
                "blockchain GET {url}: status {}",
                res.status()
            )));
        }
        let body = res
            .text()
            .await
            .map_err(|e| ProviderError::Http(format!("blockchain read body: {e}")))?;
        Self::parse(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PAYLOAD: &str =
        include_str!("../../../tests/fixtures/price/blockchain_ticker.json");

    #[test]
    fn parses_captured_payload_taking_last() {
        let quotes = BlockchainProvider::parse(SAMPLE_PAYLOAD).expect("fixture must parse");
        // Captured live 2026-06-11: ~28 majors, uppercase codes.
        assert!(quotes.len() >= 20, "expected the major-fiat set");
        assert!(quotes.contains_key("USD"));
        assert!(quotes.contains_key("EUR"));
        assert!(quotes.contains_key("JPY"));
        // No CUP/MLC (§11.4).
        assert!(!quotes.contains_key("CUP"));
        assert!(!quotes.contains_key("MLC"));
    }

    #[test]
    fn takes_last_not_buy_or_sell() {
        // `last` is mid-market; buy/sell carry the exchange spread and must
        // be ignored (§6.6 / §11.6 — the BTCPay contrast).
        let body = r#"{"USD": {"15m": 1.0, "last": 50000.0, "buy": 49000.0, "sell": 51000.0, "symbol": "USD"}}"#;
        let quotes = BlockchainProvider::parse(body).unwrap();
        assert_eq!(quotes.get("USD"), Some(&Quote::PerBtc(50_000.0)));
    }

    #[test]
    fn drops_missing_and_non_positive_last() {
        let body = r#"{
            "AAA": {"15m": 1.0, "buy": 1.0, "sell": 1.0, "symbol": "AAA"},
            "BBB": {"last": 0, "symbol": "BBB"},
            "GBP": {"last": 47293.0, "symbol": "GBP"}
        }"#;
        let quotes = BlockchainProvider::parse(body).unwrap();
        assert_eq!(quotes.len(), 1, "only GBP has a usable `last`");
        assert_eq!(quotes.get("GBP"), Some(&Quote::PerBtc(47_293.0)));
    }

    #[test]
    fn parse_error_is_returned() {
        assert!(matches!(
            BlockchainProvider::parse("not json").unwrap_err(),
            ProviderError::Parse(_)
        ));
    }

    #[test]
    fn new_strips_trailing_slash() {
        let cfg = ProviderConfig {
            enabled: true,
            url: "https://blockchain.info/".into(),
            fallback_urls: vec![],
            api_key: None,
            token: None,
            only: None,
            except: None,
        };
        assert_eq!(BlockchainProvider::new(&cfg).url, "https://blockchain.info");
    }
}
