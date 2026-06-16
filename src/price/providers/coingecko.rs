//! CoinGecko direct BTC quoter (spec §11.2).
//!
//! Calls `GET {url}/simple/price?ids=bitcoin&vs_currencies=<list>` and maps
//! the `{ "bitcoin": { ccy: price } }` body into per-currency
//! [`Quote::PerBtc`] entries. CoinGecko ships lowercase codes; the adapter
//! upper-cases them so they combine with Yadio/Blockchain quotes (spec §6.6).
//!
//! The keyless tier is rate-limited; an optional `api_key` (demo or pro)
//! raises the limits. The key is sent as the appropriate header — CoinGecko
//! pro keys go to `pro-api.coingecko.com` with `x-cg-pro-api-key`, demo keys
//! to the public host with `x-cg-demo-api-key`; we pick the header from the
//! configured URL so one config field serves both plans. Per spec §10.3 the
//! key never appears in logs or `Debug` output.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;

use crate::price::config::ProviderConfig;
use crate::price::provider::{PriceProvider, ProviderError, ProviderId, ProviderQuotes, Quote};

/// The fiat subset of CoinGecko's `supported_vs_currencies`, baked so one
/// request covers everything we can use (the endpoint requires an explicit
/// list). Codes CoinGecko does not recognise are silently omitted from the
/// response, so this list ages safely; CUP/MLC are absent because CoinGecko
/// does not list them (spec §11.2). `vef` (which CoinGecko still supports)
/// is deliberately not requested — it is the pre-redenomination Venezuelan
/// code, a different unit from the ISO `VES` (see `price::fiat`).
const VS_CURRENCIES: &str = "usd,eur,gbp,jpy,ars,aud,bdt,bhd,bmd,brl,cad,chf,\
                             clp,cny,czk,dkk,gel,hkd,huf,idr,ils,inr,krw,kwd,\
                             lkr,mmk,mxn,myr,ngn,nok,nzd,php,pkr,pln,rub,sar,\
                             sek,sgd,thb,try,twd,uah,vnd,zar";

/// Response shape: `{ "bitcoin": { "usd": 63410, ... } }`. Lenient on the
/// value (`Option<f64>`) so one `null` rate cannot fail the whole poll.
#[derive(Debug, Deserialize)]
struct CoinGeckoResponse {
    bitcoin: HashMap<String, Option<f64>>,
}

/// Direct BTC quoter against the CoinGecko API.
pub struct CoinGeckoProvider {
    url: String,
    api_key: Option<String>,
}

// Manual impl so the API key can never leak through `{:?}` logging
// (spec §10.3 redaction requirement).
impl std::fmt::Debug for CoinGeckoProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoinGeckoProvider")
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl CoinGeckoProvider {
    /// Build the provider from its `[price.providers.coingecko]` sub-table.
    pub fn new(cfg: &ProviderConfig) -> Self {
        Self {
            url: cfg.url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
        }
    }

    /// Header name for the configured key: pro keys are only valid against
    /// the `pro-api` host, demo keys against the public one.
    fn api_key_header(&self) -> &'static str {
        if self.url.contains("pro-api") {
            "x-cg-pro-api-key"
        } else {
            "x-cg-demo-api-key"
        }
    }

    /// Parse a `/simple/price` payload into [`ProviderQuotes`]. Split out
    /// from [`PriceProvider::fetch`] so it is testable against the captured
    /// fixture without HTTP (spec §10.5).
    pub(crate) fn parse(body: &str) -> Result<ProviderQuotes, ProviderError> {
        let parsed: CoinGeckoResponse = serde_json::from_str(body)
            .map_err(|e| ProviderError::Parse(format!("coingecko: {e}")))?;
        Ok(parsed
            .bitcoin
            .into_iter()
            .filter_map(|(code, value)| match value {
                Some(v) if v.is_finite() && v > 0.0 => {
                    // CoinGecko ships lowercase codes — canonicalise (§6.6).
                    Some((code.to_uppercase(), Quote::PerBtc(v)))
                }
                _ => None,
            })
            .collect())
    }
}

#[async_trait]
impl PriceProvider for CoinGeckoProvider {
    fn id(&self) -> ProviderId {
        ProviderId::CoinGecko
    }

    async fn fetch(&self, http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
        let url = format!(
            "{}/simple/price?ids=bitcoin&vs_currencies={}",
            self.url, VS_CURRENCIES
        );
        let mut req = http.get(&url);
        if let Some(key) = &self.api_key {
            req = req.header(self.api_key_header(), key);
        }
        let res = req
            .send()
            .await
            .map_err(|e| ProviderError::Http(format!("coingecko GET {url}: {e}")))?;
        if !res.status().is_success() {
            return Err(ProviderError::Http(format!(
                "coingecko GET {url}: status {}",
                res.status()
            )));
        }
        let body = res
            .text()
            .await
            .map_err(|e| ProviderError::Http(format!("coingecko read body: {e}")))?;
        Self::parse(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PAYLOAD: &str =
        include_str!("../../../tests/fixtures/price/coingecko_simple_price.json");

    fn cfg(url: &str, api_key: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            enabled: true,
            url: url.into(),
            fallback_urls: vec![],
            api_key: api_key.map(String::from),
            token: None,
            only: None,
            except: None,
        }
    }

    #[test]
    fn parses_captured_payload_and_uppercases_codes() {
        let quotes = CoinGeckoProvider::parse(SAMPLE_PAYLOAD).expect("fixture must parse");
        // Captured live 2026-06-11; codes arrive lowercase and must be
        // canonicalised so they combine with Yadio's uppercase ones (§6.6).
        assert_eq!(quotes.get("USD"), Some(&Quote::PerBtc(63410.0)));
        assert_eq!(quotes.get("EUR"), Some(&Quote::PerBtc(54815.0)));
        assert_eq!(quotes.get("JPY"), Some(&Quote::PerBtc(10143476.0)));
        assert!(!quotes.contains_key("usd"), "no lowercase keys may leak");
        // CoinGecko does not list CUP/MLC (§11.2).
        assert!(!quotes.contains_key("CUP"));
    }

    #[test]
    fn drops_null_and_non_positive() {
        let body = r#"{"bitcoin": {"usd": null, "eur": -5, "gbp": 47293.0}}"#;
        let quotes = CoinGeckoProvider::parse(body).unwrap();
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes.get("GBP"), Some(&Quote::PerBtc(47_293.0)));
    }

    #[test]
    fn parse_error_is_returned() {
        assert!(matches!(
            CoinGeckoProvider::parse("not json").unwrap_err(),
            ProviderError::Parse(_)
        ));
    }

    #[test]
    fn api_key_header_matches_host() {
        let demo = CoinGeckoProvider::new(&cfg("https://api.coingecko.com/api/v3", Some("CG-x")));
        assert_eq!(demo.api_key_header(), "x-cg-demo-api-key");
        let pro =
            CoinGeckoProvider::new(&cfg("https://pro-api.coingecko.com/api/v3", Some("CG-x")));
        assert_eq!(pro.api_key_header(), "x-cg-pro-api-key");
    }

    #[test]
    fn debug_redacts_api_key() {
        // Spec §10.3: the key must never appear in `Debug` output (logs).
        let p = CoinGeckoProvider::new(&cfg(
            "https://api.coingecko.com/api/v3",
            Some("CG-supersecret"),
        ));
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("supersecret"), "api_key leaked: {dbg}");
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn new_strips_trailing_slash() {
        let p = CoinGeckoProvider::new(&cfg("https://api.coingecko.com/api/v3/", None));
        assert_eq!(p.url, "https://api.coingecko.com/api/v3");
    }
}
