//! [`PriceManager`]: the registry + scheduler tick + read surface
//! (spec §5.3, §6.4).
//!
//! `PriceManager` owns one [`Box<dyn PriceProvider>`] per enabled provider
//! plus the aggregated-price [`PriceStore`]. The scheduler calls
//! [`PriceManager::update_all`] every `update_interval_seconds` to poll the
//! providers, aggregate, and write the store; consumers (`get_bitcoin_price`,
//! `BitcoinPriceManager::get_price`) read through [`PriceManager::get_price`].
//!
//! ## Phase 1 invariants (spec §9 Phase 1)
//! - The registry is built from `[price]`; only Yadio is wired here, the
//!   keyless backups land in Phase 2.
//! - Staleness is **logged, not enforced**: a value older than one
//!   `update_interval` emits a `warn!` but still returns to the caller, so
//!   Phase 1 never refuses an order that would have priced today.
//!   Enforcement turns on in Phase 4.
//! - Per-provider failures are isolated: a failed poll contributes nothing
//!   this tick and the store's last-known-good value is preserved (spec
//!   §6.4). The full circuit breaker integration lands in Phase 2.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use chrono::Utc;
use mostro_core::error::{MostroError, ServiceError};
use nostr_sdk::prelude::*;
use tracing::{error, info, warn};

use super::aggregate::{aggregate_tick, AggregateResult};
use super::config::{PriceSettings, ProviderConfig};
use super::provider::{PriceProvider, ProviderError, ProviderId, ProviderQuotes};
use super::providers::yadio::YadioProvider;
use super::store::{PriceError, PriceStore};

/// Process-wide singleton. Initialized once in `main` after settings load,
/// then read by the scheduler (`update_all`) and consumers (`get_price`).
/// Modelled on `MOSTRO_CONFIG`: `OnceLock` so initialization is panic-free
/// and tests that never call `init_global` see `None`.
static PRICE_MANAGER: OnceLock<PriceManager> = OnceLock::new();

/// One enabled provider plus its registry metadata. Health tracking goes
/// here in Phase 2 — Phase 1 only needs the box.
struct EnabledProvider {
    id: ProviderId,
    provider: Box<dyn PriceProvider>,
}

/// Outer `Result` is from [`tokio::time::timeout`] (Elapsed = timed out),
/// inner is the adapter's `fetch` outcome.
type TimeoutResult = Result<Result<ProviderQuotes, ProviderError>, tokio::time::error::Elapsed>;

/// Runtime state of the multi-source price module.
pub struct PriceManager {
    providers: Vec<EnabledProvider>,
    store: Arc<PriceStore>,
    settings: PriceSettings,
    http: reqwest::Client,
    /// One-shot guards for the two transient log conditions (spec §10.4
    /// asks for transitions, not per-poll spam). Kept as two independent
    /// sets so a `Stale` flag never clobbers a `SingleSource` flag (and
    /// vice versa) for the same currency — both can hold simultaneously.
    warned_stale: RwLock<HashSet<String>>,
    warned_single_source: RwLock<HashSet<String>>,
}

impl PriceManager {
    /// Build the manager from a `[price]` settings block.
    ///
    /// Validation runs first so an enabled provider with a missing required
    /// secret or an empty `url` fails fast at startup rather than silently
    /// returning no quotes (spec §7). Disabled providers are skipped; an
    /// unknown id is logged but ignored, so adding a provider in a newer
    /// release is forward-compatible with an older `mostrod` (the unknown
    /// adapter is simply absent until the binary catches up).
    pub fn from_settings(settings: PriceSettings) -> Result<Self, String> {
        settings.validate()?;

        let mut providers: Vec<EnabledProvider> = Vec::new();
        for (id_str, cfg) in &settings.providers {
            if !cfg.enabled {
                continue;
            }
            match id_str.parse::<ProviderId>() {
                Ok(id) => {
                    let provider = build_provider(id, cfg)?;
                    providers.push(EnabledProvider { id, provider });
                }
                Err(_) => {
                    warn!(
                        "price: unknown provider id `{id_str}` — ignoring (binary is older than the config?)"
                    );
                }
            }
        }

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(settings.provider_timeout_seconds))
            .user_agent(concat!("mostro/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("price: building HTTP client: {e}"))?;

        Ok(Self {
            providers,
            store: Arc::new(PriceStore::new()),
            settings,
            http,
            warned_stale: RwLock::new(HashSet::new()),
            warned_single_source: RwLock::new(HashSet::new()),
        })
    }

    /// Install the global manager. Panic-free: subsequent calls return
    /// `Err(AlreadyInstalled)` so `main` can detect (and the test suite
    /// never collides with) double-initialization.
    pub fn install_global(self) -> Result<(), InstallError> {
        PRICE_MANAGER
            .set(self)
            .map_err(|_| InstallError::AlreadyInstalled)
    }

    /// Borrow the global manager, if installed. `None` in unit tests that
    /// don't bring up the full configuration — every consumer treats that
    /// case as "no price available" rather than panicking.
    pub fn global() -> Option<&'static PriceManager> {
        PRICE_MANAGER.get()
    }

    /// Read-only view of the active settings (used by the scheduler to size
    /// its sleep and by tests).
    pub fn settings(&self) -> &PriceSettings {
        &self.settings
    }

    /// One scheduler tick: poll all enabled providers concurrently with a
    /// per-provider timeout, aggregate, and write the store
    /// (spec §5.3 steps 1–3). A failed/timed-out provider contributes
    /// nothing — the store's prior values for its currencies survive as
    /// last-known-good (spec §6.4).
    ///
    /// Returns the per-provider outcome so the scheduler / Phase 2 circuit
    /// breaker can act on it. Phase 1 only logs it.
    pub async fn update_all(&self) -> TickReport {
        let mut report = TickReport::default();
        if self.providers.is_empty() {
            warn!("price: no providers enabled — skipping tick");
            return report;
        }

        // Phase 1 only wires Yadio, so per-provider parallelism does not
        // change wall-clock time yet; each fetch is awaited in sequence
        // with its own [`tokio::time::timeout`] guard so one hanging API
        // can't block the tick beyond `provider_timeout_seconds`. Phase 2
        // (multiple direct quoters) replaces this with a concurrent driver
        // alongside the circuit-breaker integration (spec §6.5).
        let timeout = Duration::from_secs(self.settings.provider_timeout_seconds);
        let mut outcomes: Vec<(ProviderId, TimeoutResult)> =
            Vec::with_capacity(self.providers.len());
        for p in &self.providers {
            let res = tokio::time::timeout(timeout, p.provider.fetch(&self.http)).await;
            outcomes.push((p.id, res));
        }

        let mut quotes_by_provider: Vec<(ProviderId, ProviderQuotes)> =
            Vec::with_capacity(self.providers.len());
        for (id, outcome) in outcomes {
            match outcome {
                Ok(Ok(quotes)) => {
                    info!("price: {} ok ({} currencies)", id, quotes.len());
                    quotes_by_provider.push((id, quotes));
                    report.successes.push(id);
                }
                Ok(Err(e)) => {
                    warn!("price: {} error: {}", id, e);
                    report.failures.push((id, e.to_string()));
                }
                Err(_) => {
                    warn!(
                        "price: {} timed out after {}s",
                        id, self.settings.provider_timeout_seconds
                    );
                    report.failures.push((id, "timeout".to_string()));
                }
            }
        }

        // Apply per-provider currency scoping (spec §6.6) before
        // aggregation. The scoping rules are configured per
        // [price.providers.<id>]; the Phase 2 §6.6 pipeline glue (fiat
        // allowlist, etc.) layers on top of this. Doing the filter here
        // keeps `aggregate_tick` purely numeric.
        let filtered_with_ids: Vec<(ProviderId, ProviderQuotes)> = quotes_by_provider
            .into_iter()
            .map(|(id, quotes)| (id, self.scope_quotes(id, quotes)))
            .collect();

        // Contributors are providers whose **post-scope** quotes still
        // carry at least one currency — those are the only ids that can
        // actually move an aggregate this tick (spec §9 Phase 1 calls for
        // a "contributing-source list" in the Nostr `source` tag, not the
        // "polled successfully" list, which would include a provider
        // entirely filtered out by `only`/`except`). Outlier-rejected
        // individual quotes at the `combine` level are not subtracted
        // here: it would require pairing every Quote with a ProviderId
        // through `aggregate_tick`, which is an invasive change to a
        // Phase 0 pure-function module — Phase 2 may revisit if the
        // contributor list grows enough that mid-aggregate rejection is
        // common.
        let contributors: Vec<ProviderId> = filtered_with_ids
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(id, _)| *id)
            .collect();
        let filtered: Vec<ProviderQuotes> = filtered_with_ids.into_iter().map(|(_, q)| q).collect();

        let aggregates = aggregate_tick(&filtered, self.settings.outlier_threshold_pct);
        if aggregates.is_empty() {
            warn!("price: tick produced no fresh aggregates — keeping last-known-good");
            return report;
        }

        let now = Utc::now().timestamp();
        self.observe_warnings(&aggregates);
        self.store.update(aggregates.clone(), now);
        report.fresh_currencies = aggregates.len();
        report.contributors = contributors;

        if self.settings.publish_to_nostr {
            self.publish_rates_to_nostr(&aggregates, &report.contributors)
                .await;
        }

        report
    }

    /// Apply this provider's `only`/`except` filter (spec §6.6). Done at the
    /// manager boundary so the aggregator stays provider-agnostic.
    fn scope_quotes(&self, id: ProviderId, quotes: ProviderQuotes) -> ProviderQuotes {
        let cfg = match self.settings.providers.get(&id.to_string()) {
            Some(c) => c,
            None => return quotes,
        };
        if cfg.only.is_none() && cfg.except.is_none() {
            return quotes;
        }
        quotes
            .into_iter()
            .filter(|(currency, _)| cfg.allows_currency(currency))
            .collect()
    }

    /// Emit one-shot warnings on the single-source transition: a currency
    /// with one contributor warns once; gaining a second contributor
    /// clears the flag so a later regression warns again (spec §10.4).
    fn observe_warnings(&self, aggregates: &HashMap<String, AggregateResult>) {
        for (currency, agg) in aggregates {
            let key = currency.to_uppercase();
            if agg.sources <= 1 {
                if self.mark_warned(&self.warned_single_source, &key) {
                    warn!("price: {} now has a single source", currency);
                }
            } else {
                self.clear_warned(&self.warned_single_source, &key);
            }
        }
    }

    /// Read a currency's per-BTC price.
    ///
    /// Phase 1 behaviour (spec §9 Phase 1): the staleness window is checked
    /// and a `warn!` is logged on the **transition** into a stale state,
    /// but the price is still returned. The next call after a fresh tick
    /// clears the flag so future regressions warn again. Phase 4 turns
    /// this into `Err(PriceTooStale)`; doing it now would refuse orders
    /// that today's code happily prices, which is explicitly out of scope.
    pub fn get_price(&self, currency: &str) -> Result<f64, MostroError> {
        let now = Utc::now().timestamp();
        let key = currency.to_uppercase();
        match self
            .store
            .get(currency, self.settings.max_price_staleness_seconds, now)
        {
            Ok(value) => {
                self.observe_freshness(currency, &key, now);
                Ok(value)
            }
            Err(PriceError::TooStale) => {
                // Phase 1: log but still return the value — preserve the
                // legacy "never refuse" behaviour. Phase 4 will turn this
                // into a hard error.
                let snap = self.store.snapshot(currency);
                if let Some(entry) = snap {
                    let age = now.saturating_sub(entry.as_of);
                    if self.mark_warned(&self.warned_stale, &key) {
                        warn!(
                            "price: {} is past staleness window ({}s old) — Phase 1 still serves it",
                            currency, age
                        );
                    }
                    Ok(entry.value)
                } else {
                    // Should not happen: TooStale means an entry exists,
                    // but tolerate the race in case the entry was wiped
                    // between get and snapshot.
                    Err(MostroError::MostroInternalErr(ServiceError::NoAPIResponse))
                }
            }
            Err(PriceError::NoCurrency) => {
                Err(MostroError::MostroInternalErr(ServiceError::NoAPIResponse))
            }
        }
    }

    /// Test-only escape hatch so unit tests can drive the manager without
    /// the global lock. Avoid in production code; use [`Self::global`].
    #[cfg(test)]
    pub fn store(&self) -> &PriceStore {
        &self.store
    }

    /// Inspect the entry served by the `Ok` branch of [`Self::get_price`]
    /// to emit the "stale but within TTL" warning at most once, and to
    /// clear the past-TTL flag once a fresh enough value lands so the
    /// next slide past the TTL warns again.
    fn observe_freshness(&self, currency: &str, key: &str, now: i64) {
        let Some(entry) = self.store.snapshot(currency) else {
            return;
        };
        let age = now.saturating_sub(entry.as_of);
        let one_interval = self.settings.update_interval_seconds as i64;
        if age <= one_interval {
            // Fully fresh — also wipe any past-TTL flag so a future slide
            // past `max_price_staleness_seconds` warns once more.
            self.clear_warned(&self.warned_stale, key);
            return;
        }
        if self.mark_warned(&self.warned_stale, key) {
            warn!(
                "price: {} is stale ({}s old, > {}s interval)",
                currency, age, one_interval
            );
        }
    }

    /// Insert `key` into `set`; return `true` if this is the first time
    /// (the caller should warn) and `false` if the flag was already there.
    /// A poisoned lock is treated as "already warned" so callers stay
    /// quiet on lock failure rather than spamming after a panic.
    fn mark_warned(&self, set: &RwLock<HashSet<String>>, key: &str) -> bool {
        match set.write() {
            Ok(mut w) => w.insert(key.to_string()),
            Err(_) => false,
        }
    }

    fn clear_warned(&self, set: &RwLock<HashSet<String>>, key: &str) {
        if let Ok(mut w) = set.write() {
            w.remove(key);
        }
    }

    /// Publish the aggregated map to Nostr (NIP-33 kind 30078). Phase 1
    /// preserves the legacy Yadio-shaped wrapper so downstream consumers
    /// keep working byte-for-byte; the `source` tag becomes the list of
    /// **contributing** provider ids (spec §9 Phase 1: still effectively
    /// one source, but the multi-source shape is in place). Publishing is
    /// best-effort and never fails the tick.
    async fn publish_rates_to_nostr(
        &self,
        aggregates: &HashMap<String, AggregateResult>,
        successes: &[ProviderId],
    ) {
        // Build the `{"BTC": {ccy: value}}` body the legacy format used.
        let rates: HashMap<String, f64> = aggregates
            .iter()
            .map(|(c, a)| (c.clone(), a.value))
            .collect();
        let mut wrapper: HashMap<String, HashMap<String, f64>> = HashMap::new();
        wrapper.insert("BTC".to_string(), rates);

        let content = match serde_json::to_string(&wrapper) {
            Ok(c) => c,
            Err(e) => {
                error!("price: failed to serialise rates for Nostr: {e}");
                return;
            }
        };

        let keys = match crate::util::get_keys() {
            Ok(k) => k,
            Err(e) => {
                error!("price: failed to get Mostro keys for Nostr publish: {e}");
                return;
            }
        };

        let timestamp = Utc::now().timestamp();
        // Match legacy bitcoin_price.rs: 2× the interval, capped at 1h.
        let expiration_seconds = std::cmp::min(self.settings.update_interval_seconds * 2, 3600);
        let expiration = timestamp + expiration_seconds as i64;
        let source_tag = sources_to_tag(successes);
        let tags = Tags::from_list(vec![
            Tag::custom(
                TagKind::Custom("published_at".into()),
                vec![timestamp.to_string()],
            ),
            Tag::custom(TagKind::Custom("source".into()), vec![source_tag]),
            Tag::expiration(Timestamp::from(expiration as u64)),
        ]);

        let event = match crate::nip33::new_exchange_rates_event(&keys, &content, tags) {
            Ok(e) => e,
            Err(e) => {
                error!("price: failed to build exchange-rates event: {e}");
                return;
            }
        };

        let client = match crate::util::get_nostr_client() {
            Ok(c) => c,
            Err(e) => {
                error!("price: failed to get Nostr client: {e}");
                return;
            }
        };

        let timeout_duration = Duration::from_secs(30);
        match tokio::time::timeout(timeout_duration, client.send_event(&event)).await {
            Ok(Ok(output)) => info!(
                "price: published exchange rates to Nostr ({} currencies). Output: {:?}",
                aggregates.len(),
                output
            ),
            Ok(Err(e)) => error!("price: send_event to relays failed: {e}"),
            Err(_) => error!("price: timeout publishing exchange rates to Nostr (30s exceeded)"),
        }
    }
}

/// Joined list of contributing provider ids for the Nostr `source` tag.
/// Sorted so the tag is deterministic across ticks with the same provider
/// set, regardless of map-iteration order.
fn sources_to_tag(ids: &[ProviderId]) -> String {
    let mut names: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    names.sort();
    names.join(",")
}

/// Single designated extension point (spec §5.4 Step 3). Adding a new
/// provider adds exactly one match arm here — the aggregation core, the
/// store, the scheduler, and every order handler stay untouched.
fn build_provider(id: ProviderId, cfg: &ProviderConfig) -> Result<Box<dyn PriceProvider>, String> {
    match id {
        ProviderId::Yadio => Ok(Box::new(YadioProvider::new(cfg))),
        // Other adapters land in their own phases (CoinGecko/currency_api/
        // Blockchain → Phase 2, El Toque → Phase 3). Reject explicitly so
        // an over-eager config doesn't silently spawn nothing.
        ProviderId::CoinGecko
        | ProviderId::CurrencyApi
        | ProviderId::Blockchain
        | ProviderId::ElToque => Err(format!(
            "price: provider `{id}` is configured but not yet implemented in this release"
        )),
    }
}

/// Reason [`PriceManager::install_global`] refused — currently just one
/// variant, but exposed as an enum so the surface stays forward-compatible
/// without breaking callers (Phase 5 may grow shutdown/restart cases).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallError {
    /// Another `PriceManager` is already in place.
    AlreadyInstalled,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::AlreadyInstalled => f.write_str("PriceManager already installed"),
        }
    }
}

impl std::error::Error for InstallError {}

/// Per-tick outcome used by the scheduler (for outage logging) and the
/// Phase 2 circuit breaker.
#[derive(Debug, Default)]
pub struct TickReport {
    /// Providers whose [`PriceProvider::fetch`] returned `Ok` this tick.
    pub successes: Vec<ProviderId>,
    /// Providers that failed or timed out, with the stringified error.
    pub failures: Vec<(ProviderId, String)>,
    /// Providers whose post-scope quote map was non-empty — i.e. those
    /// that actually contributed at least one currency to the aggregate.
    /// Distinct from `successes`: a scoped-out provider lands in
    /// `successes` (it did poll OK) but **not** in `contributors`.
    pub contributors: Vec<ProviderId>,
    /// Number of currencies the tick produced a fresh aggregate for.
    pub fresh_currencies: usize,
}

/// Synthesise the legacy single-source config from the top-level
/// `[mostro]` block (spec §10.1). Used when `[price]` is absent so existing
/// `settings.toml` files keep working byte-for-byte.
pub fn synthesise_legacy_price_settings(
    bitcoin_price_api_url: &str,
    exchange_rates_update_interval_seconds: u64,
    publish_exchange_rates_to_nostr: bool,
) -> PriceSettings {
    let mut providers = HashMap::new();
    providers.insert(
        ProviderId::Yadio.to_string(),
        ProviderConfig {
            enabled: true,
            url: bitcoin_price_api_url.to_string(),
            fallback_urls: Vec::new(),
            api_key: None,
            token: None,
            only: None,
            except: None,
        },
    );
    PriceSettings {
        // Honour the legacy interval setting verbatim so an upgrade doesn't
        // change a node's polling cadence behind the operator's back.
        update_interval_seconds: exchange_rates_update_interval_seconds,
        publish_to_nostr: publish_exchange_rates_to_nostr,
        providers,
        ..PriceSettings::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::price::provider::{ProviderQuotes, Quote};
    use async_trait::async_trait;

    /// In-process double for the registry's `Box<dyn PriceProvider>` —
    /// drives the manager end-to-end (poll → aggregate → store) with no
    /// HTTP. Mirrors the unit-test mock in `provider.rs` but lives here so
    /// we can swap it directly into `PriceManager.providers`.
    struct ScriptedProvider {
        id: ProviderId,
        outcomes: std::sync::Mutex<Vec<Result<ProviderQuotes, ProviderError>>>,
    }

    impl ScriptedProvider {
        fn new(id: ProviderId, outcomes: Vec<Result<ProviderQuotes, ProviderError>>) -> Self {
            Self {
                id,
                outcomes: std::sync::Mutex::new(outcomes),
            }
        }
    }

    #[async_trait]
    impl PriceProvider for ScriptedProvider {
        fn id(&self) -> ProviderId {
            self.id
        }
        async fn fetch(&self, _http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
            let mut q = self.outcomes.lock().unwrap();
            if q.is_empty() {
                // Once the script runs out, behave as a healthy noop.
                return Ok(ProviderQuotes::new());
            }
            q.remove(0)
        }
    }

    fn manager_with(scripted: ScriptedProvider) -> PriceManager {
        // Disable Nostr publishing so tests don't reach the global Nostr
        // client (which isn't installed in unit tests); short timeout so a
        // hanging mock can't blow the test runner.
        let mut settings = PriceSettings {
            publish_to_nostr: false,
            provider_timeout_seconds: 5,
            ..PriceSettings::default()
        };
        settings.providers.insert(
            scripted.id.to_string(),
            ProviderConfig {
                enabled: true,
                url: "http://test".into(),
                fallback_urls: vec![],
                api_key: None,
                token: None,
                only: None,
                except: None,
            },
        );
        PriceManager {
            providers: vec![EnabledProvider {
                id: scripted.id,
                provider: Box::new(scripted),
            }],
            store: Arc::new(PriceStore::new()),
            settings,
            http: reqwest::Client::new(),
            warned_stale: RwLock::new(HashSet::new()),
            warned_single_source: RwLock::new(HashSet::new()),
        }
    }

    #[tokio::test]
    async fn single_yadio_tick_matches_today() {
        // Spec §9 Phase 1 acceptance: with only Yadio enabled, the manager
        // produces the same values as the legacy single-source path for a
        // captured sample payload.
        let mut quotes = ProviderQuotes::new();
        quotes.insert("USD".into(), Quote::PerBtc(75_899.55));
        quotes.insert("EUR".into(), Quote::PerBtc(65_393.99));
        quotes.insert("ARS".into(), Quote::PerBtc(75_899_550.0));

        let scripted = ScriptedProvider::new(ProviderId::Yadio, vec![Ok(quotes)]);
        let manager = manager_with(scripted);

        let report = manager.update_all().await;
        assert_eq!(report.successes, vec![ProviderId::Yadio]);
        assert_eq!(
            report.contributors,
            vec![ProviderId::Yadio],
            "yadio's quotes all survived scoping, so it contributes"
        );
        assert!(report.failures.is_empty());
        assert_eq!(report.fresh_currencies, 3);

        assert!(
            (manager.get_price("USD").unwrap() - 75_899.55).abs() < 1e-6,
            "USD matches Yadio's value verbatim"
        );
        assert!((manager.get_price("eur").unwrap() - 65_393.99).abs() < 1e-6);
    }

    #[tokio::test]
    async fn yadio_down_keeps_prior_values() {
        // Spec §9 Phase 1 acceptance: a failed tick must leave the store
        // intact, not wipe it. Two ticks: first succeeds, second errors.
        let mut quotes = ProviderQuotes::new();
        quotes.insert("USD".into(), Quote::PerBtc(50_000.0));
        let scripted = ScriptedProvider::new(
            ProviderId::Yadio,
            vec![Ok(quotes), Err(ProviderError::Http("down".into()))],
        );
        let manager = manager_with(scripted);

        manager.update_all().await;
        let r = manager.update_all().await;

        assert_eq!(r.successes, Vec::<ProviderId>::new());
        assert_eq!(r.failures.len(), 1);
        // Store still serves the prior tick's value — no panic, no wipe.
        assert!((manager.get_price("USD").unwrap() - 50_000.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn no_providers_returns_no_currency() {
        // Spec §9 Phase 1 acceptance: enabled=false on every provider →
        // empty store; reads return an error matching "no data yet" today.
        let settings = PriceSettings {
            publish_to_nostr: false,
            ..PriceSettings::default()
        };
        let manager = PriceManager {
            providers: vec![],
            store: Arc::new(PriceStore::new()),
            settings,
            http: reqwest::Client::new(),
            warned_stale: RwLock::new(HashSet::new()),
            warned_single_source: RwLock::new(HashSet::new()),
        };
        let r = manager.update_all().await;
        assert_eq!(r.fresh_currencies, 0);
        assert!(manager.get_price("USD").is_err());
    }

    #[tokio::test]
    async fn scoping_only_keeps_in_scope_currencies() {
        // The El-Toque-style `only` filter is implemented here even though
        // Phase 3 brings the adapter, so the Phase 1 manager already
        // honours per-provider scoping.
        let mut quotes = ProviderQuotes::new();
        quotes.insert("USD".into(), Quote::PerBtc(50_000.0));
        quotes.insert("CUP".into(), Quote::PerBtc(20_000_000.0));

        let scripted = ScriptedProvider::new(ProviderId::Yadio, vec![Ok(quotes)]);
        let mut manager = manager_with(scripted);
        manager
            .settings
            .providers
            .get_mut(&ProviderId::Yadio.to_string())
            .unwrap()
            .only = Some(vec!["CUP".into()]);

        manager.update_all().await;
        assert!(manager.get_price("CUP").is_ok());
        assert!(manager.get_price("USD").is_err());
    }

    #[test]
    fn synthesise_legacy_builds_single_yadio_provider() {
        let cfg = synthesise_legacy_price_settings("https://api.yadio.io", 600, false);
        assert_eq!(cfg.update_interval_seconds, 600);
        assert!(!cfg.publish_to_nostr);
        let yadio = cfg
            .providers
            .get("yadio")
            .expect("legacy migration must enable yadio");
        assert!(yadio.enabled);
        assert_eq!(yadio.url, "https://api.yadio.io");
        cfg.validate().expect("synthesised config must validate");
    }

    #[test]
    fn from_settings_rejects_invalid_provider_id() {
        // An enabled provider whose adapter isn't yet wired must fail at
        // startup, not silently produce nothing.
        let mut settings = PriceSettings::default();
        settings.providers.insert(
            ProviderId::CoinGecko.to_string(),
            ProviderConfig {
                enabled: true,
                url: "https://api.coingecko.com/api/v3".into(),
                fallback_urls: vec![],
                api_key: None,
                token: None,
                only: None,
                except: None,
            },
        );
        assert!(PriceManager::from_settings(settings).is_err());
    }

    #[test]
    fn from_settings_ignores_unknown_id() {
        // Adding a new provider in a newer release should not break an
        // older mostrod reading the same config — unknown ids are logged
        // and ignored.
        let mut settings = PriceSettings::default();
        settings.providers.insert(
            "future_provider".to_string(),
            ProviderConfig {
                enabled: true,
                url: "http://x".into(),
                fallback_urls: vec![],
                api_key: None,
                token: None,
                only: None,
                except: None,
            },
        );
        let m = PriceManager::from_settings(settings).expect("unknown id is non-fatal");
        assert!(m.providers.is_empty());
    }

    #[test]
    fn from_settings_skips_disabled_providers() {
        let mut settings = PriceSettings::default();
        settings.providers.insert(
            ProviderId::Yadio.to_string(),
            ProviderConfig {
                enabled: false,
                url: "https://api.yadio.io".into(),
                fallback_urls: vec![],
                api_key: None,
                token: None,
                only: None,
                except: None,
            },
        );
        let m = PriceManager::from_settings(settings).unwrap();
        assert!(m.providers.is_empty());
    }

    #[test]
    fn sources_to_tag_is_deterministic() {
        let tag = sources_to_tag(&[ProviderId::CoinGecko, ProviderId::Yadio]);
        assert_eq!(tag, "coingecko,yadio");
    }

    #[tokio::test]
    async fn scoped_out_provider_is_success_but_not_contributor() {
        // A successful poll whose every currency is filtered by `only`
        // contributes nothing to the aggregate — it must land in
        // `report.successes` (it did poll OK and circuit breaker stays
        // happy in Phase 2) but **not** in `report.contributors`, so the
        // Nostr `source` tag never names a provider that didn't move the
        // aggregate (spec §9 Phase 1: "contributing-source list").
        let mut quotes = ProviderQuotes::new();
        quotes.insert("USD".into(), Quote::PerBtc(50_000.0));
        let scripted = ScriptedProvider::new(ProviderId::Yadio, vec![Ok(quotes)]);
        let mut manager = manager_with(scripted);
        // Yadio is restricted to MLC — none of its quotes match.
        manager
            .settings
            .providers
            .get_mut(&ProviderId::Yadio.to_string())
            .unwrap()
            .only = Some(vec!["MLC".into()]);

        let report = manager.update_all().await;
        assert_eq!(report.successes, vec![ProviderId::Yadio]);
        assert!(
            report.contributors.is_empty(),
            "scoped-out provider must not appear in the Nostr source tag"
        );
        assert_eq!(report.fresh_currencies, 0);
    }

    #[tokio::test]
    async fn stale_warning_is_one_shot_then_re_arms_on_fresh_read() {
        // Build a manager whose only stored value is intentionally past
        // the TTL, then call get_price() many times: the warned_stale set
        // must grow by at most one entry. A fresh tick clears the flag
        // so a future regression past TTL warns again.
        let mut quotes = ProviderQuotes::new();
        quotes.insert("USD".into(), Quote::PerBtc(50_000.0));
        let scripted = ScriptedProvider::new(ProviderId::Yadio, vec![Ok(quotes.clone())]);
        let mut manager = manager_with(scripted);
        // Force the TTL low so the manually-written `as_of` is past it.
        manager.settings.max_price_staleness_seconds = 1;
        manager.settings.update_interval_seconds = 1;

        // Seed an explicitly-stale entry.
        let mut agg = HashMap::new();
        agg.insert(
            "USD".to_string(),
            AggregateResult {
                value: 50_000.0,
                sources: 1,
            },
        );
        // 1_000_000s ago: well past any plausible TTL.
        let now = Utc::now().timestamp();
        manager.store.update(agg, now - 1_000_000);

        // 10 reads against a stale value: warned_stale must end with
        // exactly one entry, regardless of how many times the legacy
        // code would have logged.
        for _ in 0..10 {
            let _ = manager.get_price("USD");
        }
        assert_eq!(
            manager.warned_stale.read().unwrap().len(),
            1,
            "TooStale must warn at most once between fresh reads"
        );

        // Inject a fresh tick: the `Ok` branch fires and clears the flag,
        // so a subsequent regression past TTL warns once more.
        let mut fresh = HashMap::new();
        fresh.insert(
            "USD".to_string(),
            AggregateResult {
                value: 50_000.0,
                sources: 1,
            },
        );
        let fresh_now = Utc::now().timestamp();
        manager.store.update(fresh, fresh_now);
        // Read once at fresh time so observe_freshness clears the flag.
        let _ = manager.get_price("USD");
        assert!(
            manager.warned_stale.read().unwrap().is_empty(),
            "fresh read must re-arm the stale guard"
        );
    }
}
