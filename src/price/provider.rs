//! The provider interface (spec §5.2) and per-provider health tracking
//! (spec §6.5).
//!
//! Every price API is a [`PriceProvider`] implementation living in its own
//! file under `providers/` (added from Phase 1 on). `mostrod` only ever
//! holds `Vec<Box<dyn PriceProvider>>`, so the aggregation core and the
//! scheduler stay provider-agnostic — adding an API never touches them
//! (spec §5.4).

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

/// A single currency quote from a provider.
///
/// Two flavours (spec §5.2): a **direct** quoter reports [`Quote::PerBtc`]
/// (Yadio, CoinGecko); a **fiat-cross** quoter reports
/// [`Quote::PerBase`] (El Toque: CUP per USD), resolved to a per-BTC
/// figure against the aggregated `base`/BTC anchor (spec §6.3).
#[derive(Debug, Clone, PartialEq)]
pub enum Quote {
    /// Fiat units per 1 BTC. Directly aggregatable.
    PerBtc(f64),
    /// `value` units of this currency per 1 unit of `base` currency.
    PerBase { base: String, value: f64 },
}

/// Result of one provider poll: currency code → quote.
pub type ProviderQuotes = HashMap<String, Quote>;

/// Stable identifier for a provider — used in logs, config keys, health
/// tracking, and the Nostr `source` metadata. The string form (via
/// [`fmt::Display`] / [`FromStr`]) matches the `[price.providers.<id>]`
/// config sub-table key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderId {
    Yadio,
    CoinGecko,
    CurrencyApi,
    Blockchain,
    ElToque,
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProviderId::Yadio => "yadio",
            ProviderId::CoinGecko => "coingecko",
            ProviderId::CurrencyApi => "currency_api",
            ProviderId::Blockchain => "blockchain",
            ProviderId::ElToque => "eltoque",
        };
        f.write_str(s)
    }
}

impl FromStr for ProviderId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "yadio" => Ok(ProviderId::Yadio),
            "coingecko" => Ok(ProviderId::CoinGecko),
            "currency_api" => Ok(ProviderId::CurrencyApi),
            "blockchain" => Ok(ProviderId::Blockchain),
            "eltoque" => Ok(ProviderId::ElToque),
            other => Err(format!("unknown price provider id: {other}")),
        }
    }
}

/// A provider poll failure. A failed poll contributes **nothing** to the
/// tick — never a partial map with bogus values (spec §5.2).
#[derive(Debug)]
pub enum ProviderError {
    /// Transport / HTTP-level failure (timeout, connection refused, non-2xx).
    Http(String),
    /// The body could not be parsed into the expected shape.
    Parse(String),
    /// The provider is enabled but mis-configured (e.g. a required token is
    /// missing). Surfaced at startup, not silently swallowed.
    Misconfigured(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::Http(e) => write!(f, "http error: {e}"),
            ProviderError::Parse(e) => write!(f, "parse error: {e}"),
            ProviderError::Misconfigured(e) => write!(f, "misconfigured: {e}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// One price source behind a uniform interface.
///
/// Object-safe via `#[async_trait]` so the registry can hold
/// `Vec<Box<dyn PriceProvider>>`.
#[async_trait::async_trait]
pub trait PriceProvider: Send + Sync {
    /// Stable identifier (also the config key).
    fn id(&self) -> ProviderId;

    /// Fetch the latest quotes. Returns only the currencies this provider
    /// reports; any network/parse failure is an `Err` so the provider is
    /// skipped for this tick.
    async fn fetch(&self, http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError>;
}

/// Per-provider circuit-breaker state (spec §6.5).
///
/// After `failure_threshold` consecutive failures the provider is skipped
/// for a cooldown that backs off exponentially from `base_cooldown_secs`
/// up to `cooldown_cap_secs`. A single success resets the breaker. The
/// type is pure (a clock value is always passed in) so it is wired into
/// the scheduler tick in Phase 2 and unit-testable here.
#[derive(Debug, Clone, Default)]
pub struct ProviderHealth {
    consecutive_failures: u32,
    /// Unix timestamp the provider is skipped *until*; `None` ⇒ available.
    open_until: Option<i64>,
}

impl ProviderHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when the provider should be polled this tick.
    pub fn is_available(&self, now: i64) -> bool {
        match self.open_until {
            Some(until) => now >= until,
            None => true,
        }
    }

    /// Record a successful poll: reset the breaker.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.open_until = None;
    }

    /// Record a failed poll. Once `failure_threshold` consecutive failures
    /// accumulate, open the breaker for an exponentially-backed-off
    /// cooldown capped at `cooldown_cap_secs`.
    pub fn record_failure(
        &mut self,
        now: i64,
        failure_threshold: u32,
        base_cooldown_secs: u64,
        cooldown_cap_secs: u64,
    ) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures < failure_threshold.max(1) {
            return;
        }
        // Exponent grows with each failure beyond the threshold:
        // threshold → base, threshold+1 → 2×base, threshold+2 → 4×base, …
        let over = self.consecutive_failures - failure_threshold.max(1);
        let factor = 1u64.checked_shl(over.min(63)).unwrap_or(u64::MAX);
        let cooldown = base_cooldown_secs
            .saturating_mul(factor)
            .min(cooldown_cap_secs);
        // Clamp the u64 cooldown into i64 so an absurd config cap can't wrap
        // to a negative offset and push `open_until` into the past.
        let cooldown = i64::try_from(cooldown).unwrap_or(i64::MAX);
        self.open_until = Some(now.saturating_add(cooldown));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory [`PriceProvider`] double (spec §10.5). Exercises the trait
    /// without any network: `fetch` returns canned quotes (or a forced
    /// error). Constructing a `reqwest::Client` to pass in performs no I/O.
    struct MockProvider {
        id: ProviderId,
        quotes: ProviderQuotes,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl PriceProvider for MockProvider {
        fn id(&self) -> ProviderId {
            self.id
        }
        async fn fetch(&self, _http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
            if self.fail {
                Err(ProviderError::Http("mock failure".into()))
            } else {
                Ok(self.quotes.clone())
            }
        }
    }

    #[test]
    fn provider_id_string_roundtrip() {
        for id in [
            ProviderId::Yadio,
            ProviderId::CoinGecko,
            ProviderId::CurrencyApi,
            ProviderId::Blockchain,
            ProviderId::ElToque,
        ] {
            let s = id.to_string();
            assert_eq!(ProviderId::from_str(&s).unwrap(), id, "roundtrip {s}");
        }
        assert!(ProviderId::from_str("nope").is_err());
    }

    #[tokio::test]
    async fn mock_provider_returns_canned_quotes() {
        let mut quotes = ProviderQuotes::new();
        quotes.insert("USD".to_string(), Quote::PerBtc(50_000.0));
        let p = MockProvider {
            id: ProviderId::Yadio,
            quotes: quotes.clone(),
            fail: false,
        };
        let client = reqwest::Client::new();
        assert_eq!(p.id(), ProviderId::Yadio);
        assert_eq!(p.fetch(&client).await.unwrap(), quotes);
    }

    #[tokio::test]
    async fn mock_provider_failure_is_err() {
        let p = MockProvider {
            id: ProviderId::CoinGecko,
            quotes: ProviderQuotes::new(),
            fail: true,
        };
        let client = reqwest::Client::new();
        assert!(p.fetch(&client).await.is_err());
    }

    #[test]
    fn health_stays_available_below_threshold() {
        let mut h = ProviderHealth::new();
        assert!(h.is_available(0));
        // 2 failures, threshold 3 → still available.
        h.record_failure(0, 3, 120, 1800);
        h.record_failure(0, 3, 120, 1800);
        assert!(h.is_available(0));
    }

    #[test]
    fn health_opens_at_threshold_and_recovers_after_cooldown() {
        let mut h = ProviderHealth::new();
        for _ in 0..3 {
            h.record_failure(1_000, 3, 120, 1800);
        }
        // Opened: base cooldown 120s from now.
        assert!(!h.is_available(1_000));
        assert!(!h.is_available(1_119));
        assert!(h.is_available(1_120));
    }

    #[test]
    fn health_backoff_is_exponential_and_capped() {
        let mut h = ProviderHealth::new();
        // threshold=1 so each failure opens; base 100, cap 250.
        h.record_failure(0, 1, 100, 250); // 1st: 100
        assert!(!h.is_available(99));
        assert!(h.is_available(100));
        h.record_failure(100, 1, 100, 250); // 2nd: 200
        assert!(!h.is_available(299));
        assert!(h.is_available(300));
        h.record_failure(300, 1, 100, 250); // 3rd: 400 → capped at 250
        assert!(!h.is_available(549));
        assert!(h.is_available(550));
    }

    #[test]
    fn health_success_resets_breaker() {
        let mut h = ProviderHealth::new();
        for _ in 0..5 {
            h.record_failure(0, 3, 120, 1800);
        }
        assert!(!h.is_available(0));
        h.record_success();
        assert!(h.is_available(0));
    }
}
