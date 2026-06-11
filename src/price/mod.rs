//! Multi-source BTC/fiat price module (see `docs/PRICE_PROVIDERS.md`).
//!
//! ## Phase 1
//! The module is wired into the daemon: [`PriceManager`] builds the
//! provider registry from `[price]` (or a legacy migration when the
//! section is absent, spec §10.1), the scheduler drives
//! [`PriceManager::update_all`] every `update_interval_seconds`, and
//! [`get_bitcoin_price`] / `BitcoinPriceManager::get_price` read through
//! the manager.
//!
//! ## Phase 2
//! The system is genuinely multi-source: the keyless direct backups
//! ([`providers::coingecko`], [`providers::currency_api`],
//! [`providers::blockchain`]) join [`providers::yadio`]; providers are
//! polled **concurrently** with a per-provider timeout and circuit
//! breaker (spec §6.5), and the §6.6 pipeline glue (code canonicalisation,
//! the [`fiat`] allowlist, per-provider `only`/`except` scoping) sits
//! between the adapters and the aggregation core. El Toque lands in
//! Phase 3.

pub mod aggregate;
pub mod config;
pub mod fiat;
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

/// Read a currency's per-BTC price from the global [`PriceManager`].
///
/// This is the Phase 1 entry point for consumers (`util::get_bitcoin_price`
/// and the `BitcoinPriceManager::get_price` shim). When the global manager
/// has not been initialised (e.g. unit tests that don't bring up the full
/// configuration), it returns `Err(NoAPIResponse)` — the same error the
/// legacy code returned when `BITCOIN_PRICES` was empty, so callers behave
/// identically.
pub fn get_bitcoin_price(currency: &str) -> Result<f64, MostroError> {
    match PriceManager::global() {
        Some(m) => m.get_price(currency),
        None => Err(MostroError::MostroInternalErr(ServiceError::NoAPIResponse)),
    }
}
