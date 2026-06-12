//! Yadio direct BTC quoter (spec §11.1).
//!
//! Calls `GET {url}/exrates/BTC` and maps the `{"BTC": { ccy: price }}`
//! body into per-currency [`Quote::PerBtc`] entries. Yadio occasionally
//! reports `null` for currencies it currently has no rate for (e.g.
//! `"BGN": null`); the parse is lenient (`Option<f64>`) and those entries
//! are filtered out before they leave the adapter, matching the behaviour
//! of the legacy `BitcoinPriceManager`.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;

use crate::price::config::ProviderConfig;
use crate::price::provider::{PriceProvider, ProviderError, ProviderId, ProviderQuotes, Quote};

/// Lenient response shape — `null` rates are dropped before aggregation.
#[derive(Debug, Deserialize)]
struct YadioResponse {
    #[serde(rename = "BTC")]
    btc: HashMap<String, Option<f64>>,
}

/// Direct BTC quoter against the Yadio API.
pub struct YadioProvider {
    url: String,
}

impl YadioProvider {
    /// Build the provider from its `[price.providers.yadio]` sub-table.
    pub fn new(cfg: &ProviderConfig) -> Self {
        Self {
            url: cfg.url.trim_end_matches('/').to_string(),
        }
    }

    /// Parse a Yadio `/exrates/BTC` payload into [`ProviderQuotes`].
    ///
    /// Split out from [`PriceProvider::fetch`] so the parsing path can be
    /// unit-tested against captured fixtures without standing up an HTTP
    /// server (spec §10.5).
    pub(crate) fn parse(body: &str) -> Result<ProviderQuotes, ProviderError> {
        let parsed: YadioResponse =
            serde_json::from_str(body).map_err(|e| ProviderError::Parse(format!("yadio: {e}")))?;
        Ok(parsed
            .btc
            .into_iter()
            .filter_map(|(code, value)| match value {
                Some(v) if v.is_finite() && v > 0.0 => Some((code, Quote::PerBtc(v))),
                _ => None,
            })
            .collect())
    }
}

#[async_trait]
impl PriceProvider for YadioProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Yadio
    }

    async fn fetch(&self, http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
        let url = format!("{}/exrates/BTC", self.url);
        let res = http
            .get(&url)
            .send()
            .await
            .map_err(|e| ProviderError::Http(format!("yadio GET {url}: {e}")))?;
        if !res.status().is_success() {
            return Err(ProviderError::Http(format!(
                "yadio GET {url}: status {}",
                res.status()
            )));
        }
        let body = res
            .text()
            .await
            .map_err(|e| ProviderError::Http(format!("yadio read body: {e}")))?;
        Self::parse(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PAYLOAD: &str = include_str!("../../../tests/fixtures/price/yadio_btc.json");

    #[test]
    fn parses_captured_payload() {
        let quotes = YadioProvider::parse(SAMPLE_PAYLOAD).expect("fixture must parse");
        // The captured payload contains USD, EUR, ARS, CUP plus a `null`
        // BGN to exercise the lenient path (regression for the live Yadio
        // behaviour fixed in db99f94).
        assert_eq!(quotes.get("USD"), Some(&Quote::PerBtc(75899.55)));
        assert_eq!(quotes.get("EUR"), Some(&Quote::PerBtc(65393.99)));
        assert_eq!(quotes.get("ARS"), Some(&Quote::PerBtc(75899550.0)));
        assert_eq!(quotes.get("CUP"), Some(&Quote::PerBtc(28000000.0)));
        assert!(
            !quotes.contains_key("BGN"),
            "null rates must be dropped, not surfaced as zero"
        );
    }

    #[test]
    fn drops_non_finite_and_non_positive() {
        let body = r#"{"BTC": {"USD": 0, "EUR": -1, "GBP": 50000.0}}"#;
        let quotes = YadioProvider::parse(body).unwrap();
        assert_eq!(quotes.len(), 1, "only GBP is a usable rate");
        assert_eq!(quotes.get("GBP"), Some(&Quote::PerBtc(50_000.0)));
    }

    #[test]
    fn parse_error_is_returned() {
        let err = YadioProvider::parse("not json").unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn new_strips_trailing_slash() {
        let cfg = ProviderConfig {
            enabled: true,
            url: "https://api.yadio.io/".into(),
            fallback_urls: vec![],
            api_key: None,
            token: None,
            only: None,
            except: None,
        };
        let p = YadioProvider::new(&cfg);
        // We rebuild the request URL by appending `/exrates/BTC`; without
        // stripping the trailing slash we'd hit `//exrates/BTC`.
        assert_eq!(p.url, "https://api.yadio.io");
    }
}
