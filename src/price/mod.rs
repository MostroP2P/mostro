//! Multi-source BTC/fiat price module (see `docs/PRICE_PROVIDERS.md`).
//!
//! ## Phase 1
//! The module is now wired into the daemon: [`PriceManager`] builds the
//! provider registry from `[price]` (or a legacy migration when the
//! section is absent, spec §10.1), the scheduler drives
//! [`PriceManager::update_all`] every `update_interval_seconds`, and
//! [`get_bitcoin_price`] / `BitcoinPriceManager::get_price` read through
//! the manager. The only adapter wired so far is [`providers::yadio`];
//! the keyless backups (CoinGecko, currency-api, Blockchain) land in
//! Phase 2, and El Toque in Phase 3.

pub mod aggregate;
pub mod config;
pub mod manager;
pub mod provider;
pub mod providers;
pub mod store;

pub use aggregate::{aggregate_tick, combine, resolve_per_base, AggregateResult};
pub use config::{PriceSettings, ProviderConfig};
pub use manager::{synthesise_legacy_price_settings, PriceManager, TickReport};
pub use provider::{
    PriceProvider, ProviderError, ProviderHealth, ProviderId, ProviderQuotes, Quote,
};
pub use store::{AggregatedPrice, PriceError, PriceStore};

use mostro_core::error::{MostroError, ServiceError};

/// Test-only per-currency price overrides consulted by
/// [`get_bitcoin_price`] before the global manager. Unit tests in other
/// modules (e.g. range-order bond sizing in `util.rs`) seed this via
/// `BitcoinPriceManager::set_price_for_test` instead of installing the
/// global [`PriceManager`] — its `OnceLock` would leak one test's
/// configuration into every other test in the binary. Tests use a unique
/// currency code each to avoid cross-test interference on the shared map.
#[cfg(test)]
pub(crate) fn test_price_overrides(
) -> &'static std::sync::RwLock<std::collections::HashMap<String, f64>> {
    static OVERRIDES: std::sync::OnceLock<
        std::sync::RwLock<std::collections::HashMap<String, f64>>,
    > = std::sync::OnceLock::new();
    OVERRIDES.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// Read a currency's per-BTC price from the global [`PriceManager`].
///
/// This is the Phase 1 entry point for consumers (`util::get_bitcoin_price`
/// and the `BitcoinPriceManager::get_price` shim). When the global manager
/// has not been initialised (e.g. unit tests that don't bring up the full
/// configuration), it returns `Err(NoAPIResponse)` — the same error the
/// legacy code returned when `BITCOIN_PRICES` was empty, so callers behave
/// identically.
pub fn get_bitcoin_price(currency: &str) -> Result<f64, MostroError> {
    #[cfg(test)]
    if let Some(price) = test_price_overrides()
        .read()
        .expect("price override read lock")
        .get(currency)
    {
        return Ok(*price);
    }
    match PriceManager::global() {
        Some(m) => m.get_price(currency),
        None => Err(MostroError::MostroInternalErr(ServiceError::NoAPIResponse)),
    }
}
