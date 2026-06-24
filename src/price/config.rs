//! Typed `[price]` configuration (spec §7).
//!
//! `PriceSettings` is attached to the global `Settings` as
//! `Option<PriceSettings>` (absent section ≡ legacy single-source
//! behaviour, synthesised in Phase 1's migration). Each
//! `[price.providers.<id>]` sub-table deserialises into a generic
//! [`ProviderConfig`]; the registry (Phase 1+) maps the known id strings to
//! their adapters, so adding a provider is config-only on this side.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Top-level `[price]` configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PriceSettings {
    /// How often to poll providers and recompute the aggregate.
    #[serde(default = "default_update_interval_seconds")]
    pub update_interval_seconds: u64,
    /// Serve a currency's last-known-good value up to this age; then refuse.
    #[serde(default = "default_max_price_staleness_seconds")]
    pub max_price_staleness_seconds: i64,
    /// Discard a source whose value deviates more than this percent from the
    /// median (only applies with ≥ 3 sources for a currency).
    #[serde(default = "default_outlier_threshold_pct")]
    pub outlier_threshold_pct: f64,
    /// Per-provider request timeout.
    #[serde(default = "default_provider_timeout_seconds")]
    pub provider_timeout_seconds: u64,
    /// Consecutive failures before a provider's circuit breaker opens.
    #[serde(default = "default_provider_failure_threshold")]
    pub provider_failure_threshold: u32,
    /// Base cooldown (seconds) once the breaker opens; backs off from here.
    #[serde(default = "default_provider_failure_cooldown_seconds")]
    pub provider_failure_cooldown_seconds: u64,
    /// Publish the aggregated rates to Nostr (kind 30078).
    #[serde(default = "default_publish_to_nostr")]
    pub publish_to_nostr: bool,
    /// Per-provider sub-tables, keyed by provider id (`yadio`, `coingecko`, …).
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

/// Generic per-provider config. Known-id adapters read the fields they need.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    /// Whether this provider participates in aggregation.
    #[serde(default)]
    pub enabled: bool,
    /// Primary base URL.
    pub url: String,
    /// Ordered mirrors tried when `url` fails this tick (spec §7).
    #[serde(default)]
    pub fallback_urls: Vec<String>,
    /// Optional API key (e.g. CoinGecko demo/pro). Never logged.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Optional bearer token (e.g. El Toque). Required when that provider is
    /// enabled; never logged.
    #[serde(default)]
    pub token: Option<String>,
    /// Restrict this provider to ONLY these currencies (spec §6.6).
    #[serde(default)]
    pub only: Option<Vec<String>>,
    /// Exclude these currencies from this provider (spec §6.6).
    #[serde(default)]
    pub except: Option<Vec<String>>,
}

impl ProviderConfig {
    /// Validate a single provider's config. `only` and `except` are mutually
    /// exclusive (spec §7). Currency scoping is checked here so a typo fails
    /// fast at startup rather than silently mis-aggregating.
    pub fn validate(&self, id: &str) -> Result<(), String> {
        if self.only.is_some() && self.except.is_some() {
            return Err(format!(
                "price provider '{id}': `only` and `except` are mutually exclusive \
                 (see docs/PRICE_PROVIDERS.md §7)"
            ));
        }
        if self.enabled && self.url.trim().is_empty() {
            return Err(format!(
                "price provider '{id}': enabled provider must have a non-empty `url` \
                 (see docs/PRICE_PROVIDERS.md §7)"
            ));
        }
        Ok(())
    }

    /// Whether `currency` (any casing) is in scope for this provider after
    /// applying `only` / `except`. Used by the Phase 2 pipeline glue.
    pub fn allows_currency(&self, currency: &str) -> bool {
        let c = currency.to_uppercase();
        if let Some(only) = &self.only {
            return only.iter().any(|x| x.to_uppercase() == c);
        }
        if let Some(except) = &self.except {
            return !except.iter().any(|x| x.to_uppercase() == c);
        }
        true
    }
}

impl PriceSettings {
    /// Validate the whole `[price]` block: the outlier threshold must be a
    /// sane positive percentage, and every provider must validate.
    pub fn validate(&self) -> Result<(), String> {
        if self.update_interval_seconds == 0 {
            return Err(format!(
                "price: update_interval_seconds must be > 0, got {}",
                self.update_interval_seconds
            ));
        }
        // A non-positive TTL would make `now - as_of <= ttl` never hold, so
        // every stored price reads as `TooStale` — silently disabling all
        // price lookups once this is wired into reads (Phase 4).
        if self.max_price_staleness_seconds <= 0 {
            return Err(format!(
                "price: max_price_staleness_seconds must be > 0, got {}",
                self.max_price_staleness_seconds
            ));
        }
        if !(self.outlier_threshold_pct.is_finite()
            && self.outlier_threshold_pct > 0.0
            && self.outlier_threshold_pct <= 100.0)
        {
            return Err(format!(
                "price: outlier_threshold_pct must be in (0, 100], got {}",
                self.outlier_threshold_pct
            ));
        }
        for (id, p) in &self.providers {
            p.validate(id)?;
        }
        Ok(())
    }
}

fn default_update_interval_seconds() -> u64 {
    300
}
fn default_max_price_staleness_seconds() -> i64 {
    1800
}
fn default_outlier_threshold_pct() -> f64 {
    5.0
}
fn default_provider_timeout_seconds() -> u64 {
    10
}
fn default_provider_failure_threshold() -> u32 {
    3
}
fn default_provider_failure_cooldown_seconds() -> u64 {
    120
}
fn default_publish_to_nostr() -> bool {
    true
}

impl Default for PriceSettings {
    fn default() -> Self {
        Self {
            update_interval_seconds: default_update_interval_seconds(),
            max_price_staleness_seconds: default_max_price_staleness_seconds(),
            outlier_threshold_pct: default_outlier_threshold_pct(),
            provider_timeout_seconds: default_provider_timeout_seconds(),
            provider_failure_threshold: default_provider_failure_threshold(),
            provider_failure_cooldown_seconds: default_provider_failure_cooldown_seconds(),
            publish_to_nostr: default_publish_to_nostr(),
            providers: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let cfg = PriceSettings::default();
        assert_eq!(cfg.update_interval_seconds, 300);
        assert_eq!(cfg.max_price_staleness_seconds, 1800);
        assert_eq!(cfg.outlier_threshold_pct, 5.0);
        assert_eq!(cfg.provider_timeout_seconds, 10);
        assert_eq!(cfg.provider_failure_threshold, 3);
        assert_eq!(cfg.provider_failure_cooldown_seconds, 120);
        assert!(cfg.publish_to_nostr);
        assert!(cfg.providers.is_empty());
        cfg.validate().unwrap();
    }

    #[test]
    fn toml_parses_providers_and_applies_defaults() {
        // Wrapper mirrors how `[price]` nests under the top-level settings.
        #[derive(Deserialize)]
        struct Stub {
            price: PriceSettings,
        }
        let toml_str = r#"
[price]
update_interval_seconds = 60

[price.providers.yadio]
enabled = true
url = "https://api.yadio.io"

[price.providers.currency_api]
enabled = true
url = "https://currency-api.pages.dev/v1"
fallback_urls = ["https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api@latest/v1"]
except = ["CUP", "MLC"]

[price.providers.eltoque]
enabled = false
url = "https://tasas.eltoque.com"
token = "secret"
only = ["CUP", "MLC"]
"#;
        let parsed: Stub = toml::from_str(toml_str).unwrap();
        let p = parsed.price;
        // Overridden value + defaulted siblings.
        assert_eq!(p.update_interval_seconds, 60);
        assert_eq!(p.max_price_staleness_seconds, 1800);
        assert_eq!(p.providers.len(), 3);

        let ca = &p.providers["currency_api"];
        assert!(ca.enabled);
        assert_eq!(ca.fallback_urls.len(), 1);
        assert_eq!(ca.except.as_ref().unwrap(), &["CUP", "MLC"]);
        // currency-api official CUP is scoped out.
        assert!(!ca.allows_currency("cup"));
        assert!(ca.allows_currency("USD"));

        let et = &p.providers["eltoque"];
        assert_eq!(et.token.as_deref(), Some("secret"));
        assert!(et.allows_currency("CUP"));
        assert!(!et.allows_currency("USD"));

        p.validate().unwrap();
    }

    #[test]
    fn only_and_except_together_is_rejected() {
        let cfg = ProviderConfig {
            enabled: true,
            url: "http://x".into(),
            fallback_urls: vec![],
            api_key: None,
            token: None,
            only: Some(vec!["CUP".into()]),
            except: Some(vec!["MLC".into()]),
        };
        assert!(cfg.validate("eltoque").is_err());
    }

    #[test]
    fn out_of_range_outlier_threshold_is_rejected() {
        let with_pct = |pct: f64| PriceSettings {
            outlier_threshold_pct: pct,
            ..Default::default()
        };
        assert!(with_pct(0.0).validate().is_err());
        assert!(with_pct(150.0).validate().is_err());
        with_pct(5.0).validate().unwrap();
    }

    #[test]
    fn zero_update_interval_is_rejected() {
        let cfg = PriceSettings {
            update_interval_seconds: 0,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn non_positive_staleness_is_rejected() {
        let with_ttl = |ttl: i64| PriceSettings {
            max_price_staleness_seconds: ttl,
            ..Default::default()
        };
        // Negative TTL → every price would read as TooStale.
        assert!(with_ttl(-1).validate().is_err());
        // Zero TTL is equally broken (stale the instant after it is written).
        assert!(with_ttl(0).validate().is_err());
        with_ttl(1800).validate().unwrap();
    }

    #[test]
    fn enabled_provider_with_blank_url_is_rejected() {
        let blank = ProviderConfig {
            enabled: true,
            url: "  ".into(),
            fallback_urls: vec![],
            api_key: None,
            token: None,
            only: None,
            except: None,
        };
        assert!(blank.validate("yadio").is_err());
        // A disabled provider with a blank url is allowed (inert).
        let disabled = ProviderConfig {
            enabled: false,
            ..blank.clone()
        };
        disabled.validate("yadio").unwrap();
    }

    #[test]
    fn no_scoping_allows_everything() {
        let cfg = ProviderConfig {
            enabled: true,
            url: "http://x".into(),
            fallback_urls: vec![],
            api_key: None,
            token: None,
            only: None,
            except: None,
        };
        assert!(cfg.allows_currency("USD"));
        assert!(cfg.allows_currency("CUP"));
    }
}
