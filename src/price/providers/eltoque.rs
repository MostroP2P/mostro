//! El Toque fiat-cross quoter for CUP/MLC (spec §11.3).
//!
//! El Toque publishes the **informal Cuban market rate** as *CUP per
//! foreign unit* — it is **not** a BTC price source. So unlike the direct
//! quoters, this adapter emits [`Quote::PerBase`] quotes resolved against
//! the aggregated USD/BTC anchor (spec §6.3): CUP and MLC each need at
//! least one live direct USD source (Yadio/CoinGecko/…) to resolve.
//!
//! From a `tasas` payload denominated in CUP:
//! - **CUP** → `PerBase { base: "USD", value: cup_per_usd }` (CUP per USD,
//!   taken straight from `tasas.USD`).
//! - **MLC** → `PerBase { base: "USD", value: cup_per_usd / cup_per_mlc }`.
//!   The cross math `MLC_per_USD = (CUP per USD) / (CUP per MLC)` is done
//!   **here**, inside the adapter, so the aggregator stays generic (§11.3).
//!
//! Anchor policy: **USD only** (the spec default; the EUR second-anchor
//! fallback in §11.3 Q2 was declined for this phase). If every direct USD
//! quoter is down for a tick, CUP/MLC simply fall back to last-known-good.
//!
//! The provider is scoped to `only = ["CUP", "MLC"]` in config (§6.6); the
//! adapter independently emits only CUP/MLC, so the two agree.
//!
//! Requires a Bearer **token** (free registration). An enabled El Toque
//! provider without a token is a startup error (spec §7); the token is
//! redacted from `Debug`/logs (spec §10.3).
//!
//! ## Request
//!
//! `GET {url}/v1/trmi?date_from=…&date_to=…` with `Authorization: Bearer
//! <token>`. The endpoint requires a `[date_from, date_to]` range (wire
//! format `YYYY-MM-DD HH:MM:SS`, URL-encoded) and returns the most recent
//! rate published within it, so [`PriceProvider::fetch`] queries a rolling
//! window ending "now" (see [`LOOKBACK_HOURS`]). The response is a `tasas`
//! object mapping currency codes to CUP-denominated values, plus the
//! timestamp of the returned rate (`date`/`hour`/`minutes`/`seconds`, which
//! the parser ignores). El Toque uses `ECU` for the euro. Example:
//!
//! ```json
//! { "tasas": { "USD": 490.0, "MLC": 200.0, "ECU": 540.0, … },
//!   "date": "2022-10-27", "hour": 7, "minutes": 59, "seconds": 30 }
//! ```

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde::Deserialize;

use crate::price::config::ProviderConfig;
use crate::price::provider::{PriceProvider, ProviderError, ProviderId, ProviderQuotes, Quote};

/// How far back the `[date_from, date_to]` window reaches from "now".
///
/// `/v1/trmi` returns the most recent informal-market rate published inside
/// the requested range, so the window only needs to be wide enough that a
/// quiet day (no fresh publication) still falls within it — El Toque's TRMI
/// updates roughly daily, so a ~day-wide window resolves a recent rate rather
/// than an empty `tasas`.
///
/// **Hard cap:** the API rejects any range of 24h or more with `400 "El
/// intervalo de tiempo debe ser menor a 24 horas"`, so the window must stay
/// *strictly* under 24h. 23h leaves a 1h margin for request-construction skew
/// while still spanning roughly a full daily cycle.
const LOOKBACK_HOURS: i64 = 23;

/// `date_from`/`date_to` wire format, e.g. `2022-10-27 00:00:01`
/// (sent URL-encoded by reqwest, matching the El Toque API).
const DATE_FMT: &str = "%Y-%m-%d %H:%M:%S";

/// Response shape: `{ "tasas": { "USD": 442.0, "MLC": 210.0, … } }`.
///
/// Values are **CUP per unit** of the keyed currency. Lenient on the value
/// (`Option<f64>`) so one `null` rate cannot fail the whole poll; any other
/// top-level fields El Toque returns (date range, etc.) are ignored.
#[derive(Debug, Deserialize)]
struct ElToqueResponse {
    tasas: HashMap<String, Option<f64>>,
}

/// Fiat-cross quoter against the El Toque tasas API.
pub struct ElToqueProvider {
    url: String,
    token: String,
}

// Manual impl so the Bearer token can never leak through `{:?}` logging
// (spec §10.3 redaction requirement).
impl std::fmt::Debug for ElToqueProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElToqueProvider")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl ElToqueProvider {
    /// Build the provider from its `[price.providers.eltoque]` sub-table.
    ///
    /// Returns `Err` when the required Bearer `token` is missing or blank so
    /// an enabled-but-unconfigured El Toque fails fast at startup rather
    /// than silently producing no quotes (spec §7).
    pub fn new(cfg: &ProviderConfig) -> Result<Self, String> {
        let token = cfg
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                "price provider 'eltoque': enabled provider requires a `token` (Bearer API \
                 key) — set it or disable the provider (see docs/PRICE_PROVIDERS.md §7)"
                    .to_string()
            })?;
        Ok(Self {
            url: cfg.url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    /// Parse a `tasas` payload into CUP/MLC [`Quote::PerBase`] entries.
    ///
    /// This is the grounded, testable core (spec §10.5): it targets the
    /// confirmed CUP-denominated `tasas` shape and performs the §11.3 cross
    /// math. Both outputs hang off the USD anchor:
    ///
    /// - CUP needs `tasas.USD` (CUP per USD). Absent → emit nothing
    ///   (without a CUP/USD figure nothing here is resolvable).
    /// - MLC additionally needs `tasas.MLC` (CUP per MLC) to derive
    ///   `MLC_per_USD = cup_per_usd / cup_per_mlc`.
    pub(crate) fn parse(body: &str) -> Result<ProviderQuotes, ProviderError> {
        let parsed: ElToqueResponse = serde_json::from_str(body)
            .map_err(|e| ProviderError::Parse(format!("eltoque: {e}")))?;
        let tasas = parsed.tasas;
        let mut out = ProviderQuotes::new();

        // The CUP/USD figure anchors everything El Toque contributes.
        let cup_per_usd = match tasas.get("USD") {
            Some(Some(v)) if v.is_finite() && *v > 0.0 => *v,
            _ => return Ok(out),
        };
        out.insert(
            "CUP".to_string(),
            Quote::PerBase {
                base: "USD".to_string(),
                value: cup_per_usd,
            },
        );

        // MLC per USD = (CUP per USD) / (CUP per MLC) — derived internally so
        // the aggregator only ever sees a clean `PerBase { base: "USD" }`.
        if let Some(Some(cup_per_mlc)) = tasas.get("MLC") {
            if cup_per_mlc.is_finite() && *cup_per_mlc > 0.0 {
                let mlc_per_usd = cup_per_usd / cup_per_mlc;
                if mlc_per_usd.is_finite() && mlc_per_usd > 0.0 {
                    out.insert(
                        "MLC".to_string(),
                        Quote::PerBase {
                            base: "USD".to_string(),
                            value: mlc_per_usd,
                        },
                    );
                }
            }
        }

        Ok(out)
    }
}

#[async_trait]
impl PriceProvider for ElToqueProvider {
    fn id(&self) -> ProviderId {
        ProviderId::ElToque
    }

    /// `GET {url}/v1/trmi?date_from=…&date_to=…` with Bearer-token auth.
    ///
    /// The endpoint requires a `[date_from, date_to]` range and returns the
    /// most recent rate published within it; we query a rolling window ending
    /// "now" (see [`LOOKBACK_HOURS`]) so each poll resolves the latest TRMI.
    /// reqwest URL-encodes the `YYYY-MM-DD HH:MM:SS` params.
    async fn fetch(&self, http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
        let url = format!("{}/v1/trmi", self.url);
        let now = Utc::now();
        let from = now - Duration::hours(LOOKBACK_HOURS);
        let res = http
            .get(&url)
            .bearer_auth(&self.token)
            .query(&[
                ("date_from", from.format(DATE_FMT).to_string()),
                ("date_to", now.format(DATE_FMT).to_string()),
            ])
            .send()
            .await
            .map_err(|e| ProviderError::Http(format!("eltoque GET {url}: {e}")))?;
        if !res.status().is_success() {
            return Err(ProviderError::Http(format!(
                "eltoque GET {url}: status {}",
                res.status()
            )));
        }
        let body = res
            .text()
            .await
            .map_err(|e| ProviderError::Http(format!("eltoque read body: {e}")))?;
        Self::parse(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A captured El Toque `/v1/trmi` response (CUP-denominated `tasas`).
    const SAMPLE_PAYLOAD: &str = include_str!("../../../tests/fixtures/price/eltoque_trmi.json");

    fn cfg(url: &str, token: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            enabled: true,
            url: url.into(),
            fallback_urls: vec![],
            api_key: None,
            token: token.map(String::from),
            only: Some(vec!["CUP".into(), "MLC".into()]),
            except: None,
        }
    }

    /// Pull the `value` out of a `PerBase { base: "USD", .. }` quote, asserting
    /// the base is USD (the only anchor this phase emits).
    fn per_usd(q: &Quote) -> f64 {
        match q {
            Quote::PerBase { base, value } => {
                assert_eq!(base, "USD", "El Toque must anchor on USD");
                *value
            }
            Quote::PerBtc(_) => panic!("El Toque must emit PerBase, not PerBtc"),
        }
    }

    #[test]
    fn parses_sample_payload_into_cup_and_mlc_perbase() {
        let quotes = ElToqueProvider::parse(SAMPLE_PAYLOAD).expect("fixture must parse");
        // Only CUP and MLC — El Toque's other `tasas` entries (USD anchor,
        // ECU=EUR, crypto) are not contributed by this fiat-cross adapter.
        assert_eq!(quotes.len(), 2, "exactly CUP and MLC are emitted");

        // CUP per USD is taken straight from `tasas.USD` (490 in the sample).
        assert!((per_usd(&quotes["CUP"]) - 490.0).abs() < 1e-9);

        // MLC per USD = cup_per_usd / cup_per_mlc = 490 / 200.
        assert!((per_usd(&quotes["MLC"]) - 490.0 / 200.0).abs() < 1e-9);

        // Resolved against a USD/BTC anchor this gives sane per-BTC figures:
        // CUP/BTC = 490 × USD/BTC, MLC/BTC = (490/200) × USD/BTC — i.e. 1 MLC
        // is worth 200 CUP, matching the source. Cross-check the ratio.
        let cup_per_btc = per_usd(&quotes["CUP"]) * 50_000.0; // pretend USD/BTC
        let mlc_per_btc = per_usd(&quotes["MLC"]) * 50_000.0;
        assert!(
            (cup_per_btc / mlc_per_btc - 200.0).abs() < 1e-6,
            "1 MLC must price at 200 CUP, matching tasas"
        );
    }

    #[test]
    fn mlc_cross_math_is_cup_per_usd_over_cup_per_mlc() {
        let body = r#"{"tasas":{"USD":400.0,"MLC":250.0,"ECU":420.0}}"#;
        let quotes = ElToqueProvider::parse(body).unwrap();
        assert!((per_usd(&quotes["CUP"]) - 400.0).abs() < 1e-9);
        assert!((per_usd(&quotes["MLC"]) - 400.0 / 250.0).abs() < 1e-9);
        // ECU (El Toque's EUR) is deliberately not emitted — El Toque only
        // contributes CUP/MLC (§11.3); EUR comes from the direct quoters.
        assert!(!quotes.contains_key("EUR"));
        assert!(!quotes.contains_key("ECU"));
    }

    #[test]
    fn no_usd_anchor_emits_nothing() {
        // Without CUP/USD nothing El Toque reports can be resolved.
        let body = r#"{"tasas":{"MLC":210.0,"ECU":500.0}}"#;
        let quotes = ElToqueProvider::parse(body).unwrap();
        assert!(quotes.is_empty(), "no tasas.USD → no resolvable quotes");
    }

    #[test]
    fn non_positive_rates_are_dropped() {
        // USD present but MLC is junk → CUP still emitted, MLC dropped.
        let body = r#"{"tasas":{"USD":442.0,"MLC":0}}"#;
        let quotes = ElToqueProvider::parse(body).unwrap();
        assert_eq!(quotes.len(), 1);
        assert!(quotes.contains_key("CUP"));
        assert!(!quotes.contains_key("MLC"));

        // USD itself non-positive → nothing at all.
        let body = r#"{"tasas":{"USD":0,"MLC":210.0}}"#;
        assert!(ElToqueProvider::parse(body).unwrap().is_empty());
    }

    #[test]
    fn parse_error_is_returned() {
        assert!(matches!(
            ElToqueProvider::parse("not json").unwrap_err(),
            ProviderError::Parse(_)
        ));
    }

    #[test]
    fn new_requires_a_token() {
        // Spec §7: an enabled El Toque without a token fails fast.
        assert!(ElToqueProvider::new(&cfg("https://tasas.eltoque.com", None)).is_err());
        assert!(ElToqueProvider::new(&cfg("https://tasas.eltoque.com", Some("  "))).is_err());
        assert!(ElToqueProvider::new(&cfg("https://tasas.eltoque.com", Some("tok"))).is_ok());
    }

    #[test]
    fn debug_redacts_token() {
        // Spec §10.3: the Bearer token must never appear in `Debug` (logs).
        let p = ElToqueProvider::new(&cfg("https://tasas.eltoque.com", Some("super-secret-key")))
            .unwrap();
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("super-secret-key"), "token leaked: {dbg}");
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn new_strips_trailing_slash() {
        let p = ElToqueProvider::new(&cfg("https://tasas.eltoque.com/", Some("tok"))).unwrap();
        assert_eq!(p.url, "https://tasas.eltoque.com");
    }
}
