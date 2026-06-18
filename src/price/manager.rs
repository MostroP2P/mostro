//! [`PriceManager`]: the registry + scheduler tick + read surface
//! (spec §5.3, §6.4).
//!
//! `PriceManager` owns one [`Box<dyn PriceProvider>`] per enabled provider
//! plus the aggregated-price [`PriceStore`]. The scheduler calls
//! [`PriceManager::update_all`] every `update_interval_seconds` to poll the
//! providers, aggregate, and write the store; consumers (`get_bitcoin_price`,
//! `BitcoinPriceManager::get_price`) read through [`PriceManager::get_price`].
//!
//! ## Invariants (spec §9, Phases 1–4)
//! - The registry is built from `[price]`; the direct quoters (Yadio,
//!   CoinGecko, currency-api, Blockchain) and the El Toque fiat-cross
//!   quoter (Phase 3, CUP/MLC) are all wired.
//! - Staleness is **enforced** (Phase 4, spec §6.4): a value within
//!   `max_price_staleness_seconds` is served (with a one-shot `warn!` once
//!   it ages past one `update_interval`); a value older than the TTL is
//!   refused with `ServiceError::PriceTooStale` so an order is never priced
//!   on stale data.
//! - Per-provider failures are isolated: a failed poll contributes nothing
//!   this tick and the store's last-known-good value is preserved (spec
//!   §6.4). Providers are polled **concurrently**, each bounded by
//!   `provider_timeout_seconds`, and repeated failures open a per-provider
//!   circuit breaker with exponential-backoff cooldown (spec §6.5).
//! - The §6.6 pipeline glue (fiat allowlist + per-provider `only`/`except`
//!   scoping) runs at the manager boundary, keeping `aggregate_tick`
//!   purely numeric.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use chrono::Utc;
use mostro_core::error::{MostroError, ServiceError};
use nostr_sdk::prelude::*;
use tracing::{debug, error, info, warn};

use super::aggregate::{aggregate_tick, AggregateResult};
use super::config::{PriceSettings, ProviderConfig};
use super::fiat::is_known_fiat;
use super::provider::{PriceProvider, ProviderError, ProviderHealth, ProviderId, ProviderQuotes};
use super::providers::blockchain::BlockchainProvider;
use super::providers::coingecko::CoinGeckoProvider;
use super::providers::currency_api::CurrencyApiProvider;
use super::providers::eltoque::ElToqueProvider;
use super::providers::yadio::YadioProvider;
use super::store::{PriceError, PriceStore};

/// Hard cap on the circuit breaker's exponential-backoff cooldown
/// (spec §6.5: backs off from `provider_failure_cooldown_seconds` "up to a
/// cap (default 1800)"). Not configurable — a provider that has been down
/// for a while should still be re-probed at least every 30 minutes.
const PROVIDER_COOLDOWN_CAP_SECONDS: u64 = 1800;

/// Process-wide singleton. Initialized once in `main` after settings load,
/// then read by the scheduler (`update_all`) and consumers (`get_price`).
/// Modelled on `MOSTRO_CONFIG`: `OnceLock` so initialization is panic-free
/// and tests that never call `init_global` see `None`.
static PRICE_MANAGER: OnceLock<PriceManager> = OnceLock::new();

/// One enabled provider plus its registry metadata and circuit-breaker
/// state (spec §6.5). `health` is a `Mutex` (not `RwLock`) because every
/// access mutates; contention is nil — one scheduler tick at a time.
struct EnabledProvider {
    id: ProviderId,
    provider: Box<dyn PriceProvider>,
    health: Mutex<ProviderHealth>,
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
                    providers.push(EnabledProvider {
                        id,
                        provider,
                        health: Mutex::new(ProviderHealth::new()),
                    });
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

    /// One scheduler tick: poll all enabled, breaker-available providers
    /// **concurrently** — each fetch bounded by `provider_timeout_seconds`
    /// — then aggregate and write the store (spec §5.3 steps 1–3). A
    /// failed/timed-out provider contributes nothing — the store's prior
    /// values for its currencies survive as last-known-good (spec §6.4) —
    /// and counts against its circuit breaker (spec §6.5).
    ///
    /// Returns the per-provider outcome so the scheduler can log outage
    /// transitions.
    pub async fn update_all(&self) -> TickReport {
        let mut report = TickReport::default();
        if self.providers.is_empty() {
            warn!("price: no providers enabled — skipping tick");
            return report;
        }

        // Circuit breaker (spec §6.5): a provider in cooldown is skipped
        // outright — no request, no log spam, no tick slow-down. A poisoned
        // health lock degrades to "available": polling a sick provider too
        // often is the safer failure mode (worst case: log noise), whereas
        // never polling again would silently amputate a source.
        let now = Utc::now().timestamp();
        let mut pollable: Vec<&EnabledProvider> = Vec::with_capacity(self.providers.len());
        for p in &self.providers {
            let available = p.health.lock().map(|h| h.is_available(now)).unwrap_or(true);
            if available {
                pollable.push(p);
            } else {
                info!("price: {} skipped: cooldown (circuit breaker open)", p.id);
                report.skipped.push(p.id);
            }
        }

        // Poll concurrently (spec §5.3 "poll all healthy providers, in
        // parallel"): the tick's wall-clock is the slowest single provider,
        // never the sum. Each fetch carries its own [`tokio::time::timeout`]
        // sized by [`Self::poll_budget`] — the *per-attempt* bound is the
        // shared `reqwest` client's request timeout (`from_settings`), while
        // this outer budget covers the provider's whole mirror sequence, so
        // a hanging primary cannot starve its `fallback_urls` (they'd be
        // dead code in exactly the hung case otherwise).
        let outcomes: Vec<(ProviderId, TimeoutResult)> =
            futures::future::join_all(pollable.iter().map(|p| async move {
                let res =
                    tokio::time::timeout(self.poll_budget(p.id), p.provider.fetch(&self.http))
                        .await;
                (p.id, res)
            }))
            .await;

        let mut quotes_by_provider: Vec<(ProviderId, ProviderQuotes)> =
            Vec::with_capacity(pollable.len());
        // Re-stamp the clock: the polls above may have consumed up to a full
        // poll budget, and a breaker cooldown anchored at the *pre-poll*
        // `now` would be born already partially expired — weakening the
        // skip exactly when a slow-failing provider needs it most.
        let failed_at = Utc::now().timestamp();
        // `join_all` preserves input order, so `pollable[i]` is the provider
        // behind `outcomes[i]` — zip them to feed the breaker.
        for (p, (id, outcome)) in pollable.iter().zip(outcomes) {
            let ok = match outcome {
                Ok(Ok(quotes)) => {
                    info!("price: {} ok ({} currencies)", id, quotes.len());
                    quotes_by_provider.push((id, quotes));
                    report.successes.push(id);
                    true
                }
                Ok(Err(e)) => {
                    warn!("price: {} error: {}", id, e);
                    report.failures.push((id, e.to_string()));
                    false
                }
                Err(_) => {
                    warn!(
                        "price: {} timed out after {}s (full mirror budget)",
                        id,
                        self.poll_budget(id).as_secs()
                    );
                    report.failures.push((id, "timeout".to_string()));
                    false
                }
            };
            if let Ok(mut health) = p.health.lock() {
                if ok {
                    health.record_success();
                } else {
                    health.record_failure(
                        failed_at,
                        self.settings.provider_failure_threshold,
                        self.settings.provider_failure_cooldown_seconds,
                        PROVIDER_COOLDOWN_CAP_SECONDS,
                    );
                }
            }
        }

        // §6.6 pipeline glue, at the manager boundary so `aggregate_tick`
        // stays purely numeric:
        //  1. fiat allowlist — drop crypto/metals/non-ISO junk (e.g.
        //     currency-api's `eth`, Yadio's `XAU`) before they can form
        //     single-source aggregates or bloat the Nostr event;
        //  2. per-provider `only`/`except` scoping — a mis-marketed source
        //     (currency-api's official-rate CUP) never enters the median.
        let filtered_with_ids: Vec<(ProviderId, ProviderQuotes)> = quotes_by_provider
            .into_iter()
            .map(|(id, quotes)| {
                let before = quotes.len();
                let fiat_only: ProviderQuotes = quotes
                    .into_iter()
                    .filter(|(code, _)| is_known_fiat(code))
                    .collect();
                let dropped = before - fiat_only.len();
                if dropped > 0 {
                    debug!(
                        "price: {} dropped {} non-fiat codes (allowlist)",
                        id, dropped
                    );
                }
                (id, self.scope_quotes(id, fiat_only))
            })
            .collect();

        let aggregates = aggregate_tick(&filtered_with_ids, self.settings.outlier_threshold_pct);
        if aggregates.is_empty() {
            warn!("price: tick produced no fresh aggregates — keeping last-known-good");
            return report;
        }

        // Tick-wide Nostr contributors = union of every per-currency
        // contributor list (spec §9 Phase 1 "contributing-source list").
        // Built from `aggregate_tick`'s provenance output so the tag
        // reflects the providers whose quotes actually **survived**
        // `combine`'s outlier filter, not merely those whose post-scope
        // map was non-empty.
        let mut contributor_set: std::collections::BTreeSet<ProviderId> =
            std::collections::BTreeSet::new();
        for agg in aggregates.values() {
            contributor_set.extend(agg.contributors.iter().copied());
        }
        let contributors: Vec<ProviderId> = contributor_set.into_iter().collect();

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

    /// Wall-clock budget for one provider's poll: `provider_timeout_seconds`
    /// times the number of URLs the provider may try (primary +
    /// `fallback_urls`), plus one second of slack for inter-attempt
    /// overhead.
    ///
    /// Layering: the **per-attempt** bound is enforced by the shared
    /// `reqwest` client (`from_settings` sets
    /// `.timeout(provider_timeout_seconds)`), so a hung mirror burns one
    /// slot of this budget, not all of it. Sizing the outer
    /// [`tokio::time::timeout`] to the whole sequence keeps the §7
    /// "mirrors tried in sequence" promise alive in the hung-primary case —
    /// with a flat budget the fallbacks were dead code precisely when the
    /// primary hung rather than refused (Codex review on PR #773).
    fn poll_budget(&self, id: ProviderId) -> Duration {
        let attempts = self
            .settings
            .providers
            .get(&id.to_string())
            .map(|c| 1 + c.fallback_urls.len() as u64)
            .unwrap_or(1)
            .max(1);
        Duration::from_secs(
            self.settings
                .provider_timeout_seconds
                .saturating_mul(attempts)
                .saturating_add(1),
        )
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

    /// Read a currency's per-BTC price, enforcing the staleness window.
    ///
    /// Phase 4 behaviour (spec §6.4, §9 Phase 4): a value within
    /// `max_price_staleness_seconds` is returned (with a one-shot `warn!`
    /// once it ages past one update interval); a value older than the TTL is
    /// **refused** with `ServiceError::PriceTooStale` so a market-priced
    /// order is never priced on stale data. Consumers map that onto a
    /// user-facing `CantDoReason::PriceTooStale` at the order boundary.
    /// A currency that was never stored is `NoAPIResponse` (no data yet).
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
                // Phase 4: refuse past-TTL prices. Warn once on the
                // transition so the log isn't spammed every read while the
                // outage persists; a later fresh tick clears the flag (via
                // `observe_freshness`) so the next slide past the TTL warns
                // again.
                if self.mark_warned(&self.warned_stale, &key) {
                    let age = self
                        .store
                        .snapshot(currency)
                        .map(|e| now.saturating_sub(e.as_of));
                    match age {
                        Some(age) => warn!(
                            "price: {} is past the staleness window ({}s old) — refusing",
                            currency, age
                        ),
                        None => warn!(
                            "price: {} is past the staleness window — refusing",
                            currency
                        ),
                    }
                }
                Err(MostroError::MostroInternalErr(ServiceError::PriceTooStale))
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
        ProviderId::CoinGecko => Ok(Box::new(CoinGeckoProvider::new(cfg))),
        ProviderId::CurrencyApi => Ok(Box::new(CurrencyApiProvider::new(cfg))),
        ProviderId::Blockchain => Ok(Box::new(BlockchainProvider::new(cfg))),
        // El Toque (fiat-cross CUP/MLC). `new` returns `Err` when the
        // required Bearer token is missing, so an enabled-but-unconfigured
        // provider fails fast at startup (spec §7).
        ProviderId::ElToque => Ok(Box::new(ElToqueProvider::new(cfg)?)),
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

/// Per-tick outcome used by the scheduler (for outage logging) and tests.
#[derive(Debug, Default)]
pub struct TickReport {
    /// Providers whose [`PriceProvider::fetch`] returned `Ok` this tick.
    pub successes: Vec<ProviderId>,
    /// Providers that failed or timed out, with the stringified error.
    pub failures: Vec<(ProviderId, String)>,
    /// Providers not polled because their circuit breaker is in cooldown
    /// (spec §6.5). Neither a success nor a failure: the breaker state
    /// carries over to the next tick.
    pub skipped: Vec<ProviderId>,
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

    fn manager_with_many(scripted: Vec<ScriptedProvider>) -> PriceManager {
        // Disable Nostr publishing so tests don't reach the global Nostr
        // client (which isn't installed in unit tests); short timeout so a
        // hanging mock can't blow the test runner.
        let mut settings = PriceSettings {
            publish_to_nostr: false,
            provider_timeout_seconds: 5,
            ..PriceSettings::default()
        };
        let mut providers = Vec::new();
        for s in scripted {
            settings.providers.insert(
                s.id.to_string(),
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
            providers.push(EnabledProvider {
                id: s.id,
                provider: Box::new(s),
                health: Mutex::new(ProviderHealth::new()),
            });
        }
        PriceManager {
            providers,
            store: Arc::new(PriceStore::new()),
            settings,
            http: reqwest::Client::new(),
            warned_stale: RwLock::new(HashSet::new()),
            warned_single_source: RwLock::new(HashSet::new()),
        }
    }

    fn manager_with(scripted: ScriptedProvider) -> PriceManager {
        manager_with_many(vec![scripted])
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

    fn eltoque_cfg(token: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            enabled: true,
            url: "https://tasas.eltoque.com".into(),
            fallback_urls: vec![],
            api_key: None,
            token: token.map(String::from),
            only: Some(vec!["CUP".into(), "MLC".into()]),
            except: None,
        }
    }

    #[test]
    fn from_settings_builds_eltoque_with_token() {
        // Phase 3: El Toque is now wired. With its required Bearer token it
        // builds into the registry like any other provider.
        let mut settings = PriceSettings::default();
        settings
            .providers
            .insert(ProviderId::ElToque.to_string(), eltoque_cfg(Some("tok")));
        let m = PriceManager::from_settings(settings).expect("eltoque builds with a token");
        assert_eq!(m.providers.len(), 1);
        assert_eq!(m.providers[0].id, ProviderId::ElToque);
    }

    #[test]
    fn from_settings_rejects_eltoque_without_token() {
        // Spec §7: an enabled El Toque missing its required Bearer token must
        // fail fast at startup, not silently produce nothing.
        let mut settings = PriceSettings::default();
        settings
            .providers
            .insert(ProviderId::ElToque.to_string(), eltoque_cfg(None));
        assert!(PriceManager::from_settings(settings).is_err());
    }

    #[test]
    fn from_settings_builds_all_phase2_providers() {
        // Spec §9 Phase 2: the three keyless backups join Yadio in the
        // registry — the §7 example config must now build cleanly.
        let mut settings = PriceSettings::default();
        for (id, url) in [
            (ProviderId::Yadio, "https://api.yadio.io"),
            (ProviderId::CoinGecko, "https://api.coingecko.com/api/v3"),
            (ProviderId::CurrencyApi, "https://currency-api.pages.dev/v1"),
            (ProviderId::Blockchain, "https://blockchain.info"),
        ] {
            settings.providers.insert(
                id.to_string(),
                ProviderConfig {
                    enabled: true,
                    url: url.into(),
                    fallback_urls: vec![],
                    api_key: None,
                    token: None,
                    only: None,
                    except: None,
                },
            );
        }
        let m = PriceManager::from_settings(settings).expect("phase 2 registry builds");
        assert_eq!(m.providers.len(), 4);
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

    fn quotes_of(pairs: &[(&str, f64)]) -> ProviderQuotes {
        pairs
            .iter()
            .map(|(c, v)| (c.to_string(), Quote::PerBtc(*v)))
            .collect()
    }

    #[tokio::test]
    async fn multi_source_aggregate_is_median_plus_outlier_mean() {
        // Spec §9 Phase 2 acceptance: EUR/USD aggregate = median + outlier
        // across all live direct quoters, and a wild outlier with ≥3
        // sources is discarded. USD candidates {49_500, 50_000, 50_500,
        // 80_000}: median 50_250, the 5% band keeps the first three, the
        // 80_000 outlier is dropped → mean = 50_000.
        let providers = vec![
            ScriptedProvider::new(
                ProviderId::Yadio,
                vec![Ok(quotes_of(&[("USD", 50_000.0), ("EUR", 43_000.0)]))],
            ),
            ScriptedProvider::new(
                ProviderId::CoinGecko,
                vec![Ok(quotes_of(&[("USD", 50_500.0), ("EUR", 43_200.0)]))],
            ),
            ScriptedProvider::new(
                ProviderId::Blockchain,
                vec![Ok(quotes_of(&[("USD", 49_500.0), ("EUR", 42_800.0)]))],
            ),
            ScriptedProvider::new(
                ProviderId::CurrencyApi,
                vec![Ok(quotes_of(&[("USD", 80_000.0)]))], // wild outlier
            ),
        ];
        let manager = manager_with_many(providers);
        let report = manager.update_all().await;
        assert_eq!(report.successes.len(), 4);

        let usd = manager.get_price("USD").unwrap();
        assert!(
            (usd - 50_000.0).abs() < 1e-6,
            "outlier must be discarded before the mean, got {usd}"
        );
        let eur = manager.get_price("EUR").unwrap();
        assert!(
            (eur - 43_000.0).abs() < 1e-6,
            "median-anchored mean, got {eur}"
        );
        // The outlier provider polled OK but its value did not survive —
        // it must not appear in the contributing-source list.
        assert!(report.contributors.contains(&ProviderId::Yadio));
        assert!(!report.contributors.contains(&ProviderId::CurrencyApi));
    }

    #[tokio::test]
    async fn lowercase_and_uppercase_codes_combine() {
        // Spec §9 Phase 2 acceptance: lowercase currency-api codes combine
        // with uppercase Yadio codes — the normalisation test that would
        // silently fail without §6.6. (Adapters canonicalise; this guards
        // the aggregator-side uppercase against a future adapter that
        // forgets.)
        let providers = vec![
            ScriptedProvider::new(ProviderId::Yadio, vec![Ok(quotes_of(&[("USD", 50_000.0)]))]),
            ScriptedProvider::new(
                ProviderId::CurrencyApi,
                vec![Ok(quotes_of(&[("usd", 51_000.0)]))],
            ),
        ];
        let manager = manager_with_many(providers);
        manager.update_all().await;
        let usd = manager.get_price("USD").unwrap();
        assert!(
            (usd - 50_500.0).abs() < 1e-6,
            "two casings must form ONE two-source aggregate (mean), got {usd}"
        );
    }

    #[tokio::test]
    async fn non_fiat_codes_are_dropped_by_allowlist() {
        // Spec §9 Phase 2 acceptance: non-fiat codes (`eth`, `bnb`) from
        // currency-api are dropped by the allowlist — as are Yadio's
        // metals/BTC self-quote.
        let providers = vec![ScriptedProvider::new(
            ProviderId::CurrencyApi,
            vec![Ok(quotes_of(&[
                ("usd", 50_000.0),
                ("eth", 37.8),
                ("bnb", 150.0),
                ("xau", 25.0),
                ("btc", 1.0),
            ]))],
        )];
        let manager = manager_with_many(providers);
        let report = manager.update_all().await;
        assert_eq!(
            report.fresh_currencies, 1,
            "only USD survives the allowlist"
        );
        assert!(manager.get_price("USD").is_ok());
        assert!(manager.get_price("ETH").is_err());
        assert!(manager.get_price("BTC").is_err());
    }

    #[tokio::test]
    async fn official_cup_is_scoped_out_by_except() {
        // Spec §9 Phase 2 acceptance: currency-api's official-rate CUP is
        // scoped out by the shipped `except = ["CUP","MLC"]`, so it never
        // enters the CUP aggregate — Yadio's informal rate stands alone.
        let providers = vec![
            ScriptedProvider::new(
                ProviderId::Yadio,
                vec![Ok(quotes_of(&[("USD", 50_000.0), ("CUP", 20_000_000.0)]))],
            ),
            ScriptedProvider::new(
                ProviderId::CurrencyApi,
                // Official rate: ~26 CUP/USD → 1.3M CUP/BTC — 15× off.
                vec![Ok(quotes_of(&[("usd", 50_100.0), ("cup", 1_300_000.0)]))],
            ),
        ];
        let mut manager = manager_with_many(providers);
        manager
            .settings
            .providers
            .get_mut(&ProviderId::CurrencyApi.to_string())
            .unwrap()
            .except = Some(vec!["CUP".into(), "MLC".into()]);

        manager.update_all().await;
        let cup = manager.get_price("CUP").unwrap();
        assert!(
            (cup - 20_000_000.0).abs() < 1e-6,
            "official-rate CUP must never enter the aggregate (got {cup}); \
             with only 2 sources the outlier guard cannot save us — scoping must"
        );
        // USD still combines from both.
        let usd = manager.get_price("USD").unwrap();
        assert!((usd - 50_050.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn provider_down_falls_back_to_remaining_sources() {
        // Spec §9 Phase 2 acceptance: a provider down → currencies fall
        // back to the remaining sources, same tick.
        let providers = vec![
            ScriptedProvider::new(ProviderId::Yadio, vec![Ok(quotes_of(&[("USD", 50_000.0)]))]),
            ScriptedProvider::new(
                ProviderId::CoinGecko,
                vec![Err(ProviderError::Http("down".into()))],
            ),
        ];
        let manager = manager_with_many(providers);
        let report = manager.update_all().await;
        assert_eq!(report.successes, vec![ProviderId::Yadio]);
        assert_eq!(report.failures.len(), 1);
        assert!((manager.get_price("USD").unwrap() - 50_000.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn circuit_breaker_skips_after_threshold_failures() {
        // Spec §9 Phase 2 acceptance: the breaker opens after N consecutive
        // failures, and an open breaker means the provider is not even
        // polled next tick (skipped, not failed). The cooldown/half-open
        // timing math is covered by the pure ProviderHealth unit tests.
        let scripted = ScriptedProvider::new(
            ProviderId::CoinGecko,
            vec![
                Err(ProviderError::Http("down".into())),
                // Would succeed if (wrongly) polled while the breaker is open:
                Ok(quotes_of(&[("USD", 50_000.0)])),
            ],
        );
        let mut manager = manager_with(scripted);
        manager.settings.provider_failure_threshold = 1;
        manager.settings.provider_failure_cooldown_seconds = 3_600; // ≫ test runtime

        let first = manager.update_all().await;
        assert_eq!(
            first.failures.len(),
            1,
            "tick 1: the failure trips the breaker"
        );
        assert!(first.skipped.is_empty());

        let second = manager.update_all().await;
        assert_eq!(
            second.skipped,
            vec![ProviderId::CoinGecko],
            "tick 2: open breaker → skipped without polling"
        );
        assert!(
            second.successes.is_empty(),
            "the scripted Ok was never consumed"
        );
        assert!(second.failures.is_empty(), "skipped ≠ failed");
    }

    #[test]
    fn poll_budget_scales_with_fallback_urls() {
        // The outer per-provider timeout must cover the whole mirror
        // sequence (primary + fallbacks), or a hung primary starves the
        // mirrors (Codex review on PR #773). Per-attempt bounding is the
        // shared reqwest client's job.
        let scripted = ScriptedProvider::new(ProviderId::CurrencyApi, vec![]);
        let mut manager = manager_with(scripted);
        manager.settings.provider_timeout_seconds = 10;

        // No fallbacks: one attempt + 1s slack.
        assert_eq!(
            manager.poll_budget(ProviderId::CurrencyApi),
            Duration::from_secs(11)
        );
        // Two mirrors: three attempts + slack.
        manager
            .settings
            .providers
            .get_mut(&ProviderId::CurrencyApi.to_string())
            .unwrap()
            .fallback_urls = vec!["http://m1".into(), "http://m2".into()];
        assert_eq!(
            manager.poll_budget(ProviderId::CurrencyApi),
            Duration::from_secs(31)
        );
        // Unknown id (defensive): single-attempt budget.
        assert_eq!(
            manager.poll_budget(ProviderId::Blockchain),
            Duration::from_secs(11)
        );
    }

    #[tokio::test]
    async fn breaker_success_after_cooldown_resets() {
        // With a zero-second cooldown the breaker re-probes immediately;
        // a success must reset it (no skip on the following tick).
        let scripted = ScriptedProvider::new(
            ProviderId::CoinGecko,
            vec![
                Err(ProviderError::Http("down".into())),
                Ok(quotes_of(&[("USD", 50_000.0)])),
                Ok(quotes_of(&[("USD", 50_100.0)])),
            ],
        );
        let mut manager = manager_with(scripted);
        manager.settings.provider_failure_threshold = 1;
        manager.settings.provider_failure_cooldown_seconds = 0; // immediate re-probe

        manager.update_all().await; // fails, opens (0s cooldown)
        let second = manager.update_all().await; // re-probe succeeds → reset
        assert_eq!(second.successes, vec![ProviderId::CoinGecko]);
        let third = manager.update_all().await;
        assert_eq!(third.successes, vec![ProviderId::CoinGecko]);
        assert!(third.skipped.is_empty());
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
                contributors: vec![ProviderId::Yadio],
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
                contributors: vec![ProviderId::Yadio],
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

    #[tokio::test]
    async fn stale_price_is_refused_but_fresh_is_served() {
        // Spec §9 Phase 4: a value past the staleness window is refused with
        // `PriceTooStale`; a value within the window is still served. (Phases
        // 1–3 served the stale value too — this is the behaviour change.)
        let mut quotes = ProviderQuotes::new();
        quotes.insert("USD".into(), Quote::PerBtc(50_000.0));
        let scripted = ScriptedProvider::new(ProviderId::Yadio, vec![Ok(quotes)]);
        let mut manager = manager_with(scripted);
        manager.settings.max_price_staleness_seconds = 1_800;

        let mk = |v: f64| {
            let mut agg = HashMap::new();
            agg.insert(
                "USD".to_string(),
                AggregateResult {
                    value: v,
                    sources: 1,
                    contributors: vec![ProviderId::Yadio],
                },
            );
            agg
        };
        let now = Utc::now().timestamp();

        // Past the TTL → refused with PriceTooStale.
        manager.store.update(mk(50_000.0), now - 10_000);
        assert!(matches!(
            manager.get_price("USD"),
            Err(MostroError::MostroInternalErr(ServiceError::PriceTooStale))
        ));

        // A fresh tick brings it back within the window → served again.
        manager.store.update(mk(51_000.0), now);
        assert!((manager.get_price("USD").unwrap() - 51_000.0).abs() < 1e-6);

        // A never-seen currency is "no data", not "stale".
        assert!(matches!(
            manager.get_price("EUR"),
            Err(MostroError::MostroInternalErr(ServiceError::NoAPIResponse))
        ));
    }
}
