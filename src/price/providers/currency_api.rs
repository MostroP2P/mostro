//! currency-api / fawazahmed0 direct BTC quoter (spec §11.5).
//!
//! Calls `GET {url}/currencies/btc.min.json` and maps the
//! `{ "date": "…", "btc": { ccy: price } }` body into per-currency
//! [`Quote::PerBtc`] entries. Two §6.6 caveats are handled *outside* this
//! adapter, by design:
//!
//! - The payload ships **324+ entries including crypto** (`eth`, `bnb`, …)
//!   and non-ISO codes — the manager's fiat allowlist drops those before
//!   aggregation. The adapter itself stays a faithful map of the API.
//! - Its CUP is the **official** rate (~26 CUP/USD), a different market
//!   from Yadio/El Toque's informal rate (~400 CUP/USD) — the shipped
//!   config scopes it out with `except = ["CUP", "MLC"]`.
//!
//! Codes arrive **lowercase** and are canonicalised to uppercase here so
//! they combine with Yadio/Blockchain quotes (spec §6.6).
//!
//! The API is CDN-hosted (Cloudflare Pages + a jsdelivr mirror). The
//! adapter implements `fallback_urls` (spec §7): mirrors are tried in
//! order, and the provider only fails the tick when **every** URL fails.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;

use crate::price::config::ProviderConfig;
use crate::price::provider::{PriceProvider, ProviderError, ProviderId, ProviderQuotes, Quote};

/// Response shape: `{ "date": "…", "btc": { "usd": 62519.29, … } }`.
/// Lenient on the value so one `null` cannot fail the whole poll.
#[derive(Debug, Deserialize)]
struct CurrencyApiResponse {
    btc: HashMap<String, Option<f64>>,
}

/// Direct BTC quoter against currency-api with ordered mirror fallback.
#[derive(Debug)]
pub struct CurrencyApiProvider {
    urls: Vec<String>,
}

impl CurrencyApiProvider {
    /// Build the provider from its `[price.providers.currency_api]`
    /// sub-table. The primary `url` plus every `fallback_urls` entry form
    /// the ordered candidate list (spec §7).
    pub fn new(cfg: &ProviderConfig) -> Self {
        let urls = std::iter::once(&cfg.url)
            .chain(cfg.fallback_urls.iter())
            .map(|u| u.trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .collect();
        Self { urls }
    }

    /// Parse a `btc.min.json` payload into [`ProviderQuotes`]. Split out
    /// from [`PriceProvider::fetch`] so it is testable against the captured
    /// fixture without HTTP (spec §10.5).
    pub(crate) fn parse(body: &str) -> Result<ProviderQuotes, ProviderError> {
        let parsed: CurrencyApiResponse = serde_json::from_str(body)
            .map_err(|e| ProviderError::Parse(format!("currency_api: {e}")))?;
        Ok(parsed
            .btc
            .into_iter()
            .filter_map(|(code, value)| match value {
                Some(v) if v.is_finite() && v > 0.0 => {
                    // currency-api ships lowercase codes — canonicalise (§6.6).
                    Some((code.to_uppercase(), Quote::PerBtc(v)))
                }
                _ => None,
            })
            .collect())
    }

    /// One attempt against one base URL.
    async fn fetch_one(
        &self,
        http: &reqwest::Client,
        base: &str,
    ) -> Result<ProviderQuotes, ProviderError> {
        let url = format!("{base}/currencies/btc.min.json");
        let res = http
            .get(&url)
            .send()
            .await
            .map_err(|e| ProviderError::Http(format!("currency_api GET {url}: {e}")))?;
        if !res.status().is_success() {
            return Err(ProviderError::Http(format!(
                "currency_api GET {url}: status {}",
                res.status()
            )));
        }
        let body = res
            .text()
            .await
            .map_err(|e| ProviderError::Http(format!("currency_api read body: {e}")))?;
        Self::parse(&body)
    }
}

#[async_trait]
impl PriceProvider for CurrencyApiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::CurrencyApi
    }

    async fn fetch(&self, http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
        // Try the primary, then each mirror in order; first success wins.
        // Only when every URL fails does the provider fail the tick (and
        // count against its circuit breaker) — spec §7 "tried in sequence".
        let mut last_err =
            ProviderError::Misconfigured("currency_api: no usable url configured".into());
        for base in &self.urls {
            match self.fetch_one(http, base).await {
                Ok(quotes) => return Ok(quotes),
                Err(e) => {
                    tracing::warn!("price: currency_api mirror {base} failed: {e}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PAYLOAD: &str =
        include_str!("../../../tests/fixtures/price/currency_api_btc.json");

    fn cfg(url: &str, fallbacks: Vec<String>) -> ProviderConfig {
        ProviderConfig {
            enabled: true,
            url: url.into(),
            fallback_urls: fallbacks,
            api_key: None,
            token: None,
            only: None,
            except: None,
        }
    }

    #[test]
    fn parses_captured_payload_and_uppercases_codes() {
        let quotes = CurrencyApiProvider::parse(SAMPLE_PAYLOAD).expect("fixture must parse");
        // Captured live 2026-06-11. Lowercase codes must be canonicalised.
        assert!(quotes.contains_key("USD"));
        assert!(quotes.contains_key("EUR"));
        assert!(!quotes.contains_key("usd"), "no lowercase keys may leak");
        // The raw payload legitimately includes crypto junk (`eth`, `bnb`)
        // and the OFFICIAL-rate CUP — the adapter maps them faithfully;
        // dropping them is the job of the manager's fiat allowlist and the
        // shipped `except = ["CUP","MLC"]` scoping (§6.6). Asserting they
        // are present here pins the layering: adapter = faithful map.
        assert!(quotes.contains_key("ETH"));
        assert!(quotes.contains_key("CUP"));
        // The captured CUP is the official rate: CUP/BTC ÷ USD/BTC ≈ 26
        // CUP/USD (the informal market is ~400) — the §11.5 hazard is real.
        let cup = match quotes.get("CUP").unwrap() {
            Quote::PerBtc(v) => *v,
            _ => unreachable!(),
        };
        let usd = match quotes.get("USD").unwrap() {
            Quote::PerBtc(v) => *v,
            _ => unreachable!(),
        };
        let cup_per_usd = cup / usd;
        assert!(
            (20.0..40.0).contains(&cup_per_usd),
            "captured CUP should be the official ~26 CUP/USD rate, got {cup_per_usd}"
        );
    }

    #[test]
    fn drops_null_and_non_positive() {
        let body = r#"{"date":"2026-06-11","btc":{"usd":null,"eur":-1,"gbp":47000.5}}"#;
        let quotes = CurrencyApiProvider::parse(body).unwrap();
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes.get("GBP"), Some(&Quote::PerBtc(47_000.5)));
    }

    #[test]
    fn parse_error_is_returned() {
        assert!(matches!(
            CurrencyApiProvider::parse("not json").unwrap_err(),
            ProviderError::Parse(_)
        ));
    }

    #[test]
    fn url_order_is_primary_then_fallbacks() {
        let p = CurrencyApiProvider::new(&cfg(
            "https://currency-api.pages.dev/v1/",
            vec!["https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api@latest/v1".into()],
        ));
        assert_eq!(
            p.urls,
            vec![
                "https://currency-api.pages.dev/v1",
                "https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api@latest/v1"
            ]
        );
    }

    /// Spec §9 Phase 2 acceptance: "a provider's `fallback_urls` is tried
    /// before the provider is marked failed". The primary URL points at a
    /// dead local port (instant connection-refused); the fallback is a real
    /// local HTTP server returning the captured fixture. `fetch` must
    /// succeed via the mirror.
    #[tokio::test]
    async fn fallback_url_is_tried_before_failing() {
        use axum::{routing::get, Router};

        let app = Router::new().route(
            "/v1/currencies/btc.min.json",
            get(|| async { SAMPLE_PAYLOAD }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let p = CurrencyApiProvider::new(&cfg(
            // Port 9 (discard) on localhost: nothing listens, fails fast.
            "http://127.0.0.1:9/v1",
            vec![format!("http://{addr}/v1")],
        ));
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let quotes = p.fetch(&http).await.expect("mirror must carry the fetch");
        assert!(quotes.contains_key("USD"));
    }

    /// A *hanging* primary (vs the fast connection-refused above) must not
    /// starve the mirror: the per-attempt bound is the HTTP client's
    /// request timeout, so the hung attempt is cut at ~1s and the mirror
    /// still answers within the manager's mirror-sequence budget
    /// (`poll_budget`). Guards the Codex finding on PR #773.
    #[tokio::test]
    async fn hanging_primary_does_not_starve_the_mirror() {
        use axum::{routing::get, Router};

        // Primary: accepts the connection, then stalls far past the client
        // request timeout.
        let hang = Router::new().route(
            "/v1/currencies/btc.min.json",
            get(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                SAMPLE_PAYLOAD
            }),
        );
        let hang_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hang_addr = hang_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(hang_listener, hang).await.unwrap();
        });

        // Mirror: instant fixture.
        let ok = Router::new().route(
            "/v1/currencies/btc.min.json",
            get(|| async { SAMPLE_PAYLOAD }),
        );
        let ok_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ok_addr = ok_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(ok_listener, ok).await.unwrap();
        });

        let p = CurrencyApiProvider::new(&cfg(
            &format!("http://{hang_addr}/v1"),
            vec![format!("http://{ok_addr}/v1")],
        ));
        // Mirrors `from_settings`: the client's request timeout IS the
        // per-attempt bound.
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        let quotes = p
            .fetch(&http)
            .await
            .expect("mirror must carry the fetch despite the hung primary");
        assert!(quotes.contains_key("USD"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "hung primary must be cut by the per-attempt timeout, not ride forever"
        );
    }

    #[tokio::test]
    async fn all_urls_failing_is_one_provider_error() {
        let p = CurrencyApiProvider::new(&cfg(
            "http://127.0.0.1:9/v1",
            vec!["http://127.0.0.1:9/v2".into()],
        ));
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        assert!(matches!(
            p.fetch(&http).await.unwrap_err(),
            ProviderError::Http(_)
        ));
    }
}
