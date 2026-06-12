//! Legacy `BitcoinPriceManager` shim (spec §9 Phase 1 / §10.1).
//!
//! The real price logic lives in [`crate::price`] from Phase 1 onward.
//! This module survives only as a thin `get_price` delegate so any
//! downstream caller still referring to `BitcoinPriceManager` keeps
//! compiling; the type itself is scheduled for removal in Phase 5 once
//! every consumer reads through `PriceManager` directly.
#![allow(dead_code)]

use mostro_core::prelude::*;

pub struct BitcoinPriceManager;

impl BitcoinPriceManager {
    /// Delegates to [`crate::price::get_bitcoin_price`]. Behaviour is
    /// identical to the legacy implementation: an uppercase ISO-4217 code
    /// returns the per-BTC value if the global [`crate::price::PriceManager`]
    /// has it, otherwise `Err(NoAPIResponse)` (matching "no data yet" in
    /// the pre-Phase-1 world).
    pub fn get_price(currency: &str) -> Result<f64, MostroError> {
        crate::price::get_bitcoin_price(currency)
    }

    /// Test-only: seed the price-override map consulted by
    /// [`crate::price::get_bitcoin_price`] so unit tests in other modules
    /// can exercise price-dependent paths (e.g. range-order bond sizing)
    /// deterministically without hitting the network or installing the
    /// global `PriceManager`. Use a unique `currency` per test to avoid
    /// cross-test interference on the shared map.
    #[cfg(test)]
    pub(crate) fn set_price_for_test(currency: &str, price: f64) {
        crate::price::test_price_overrides()
            .write()
            .expect("price override write lock")
            .insert(currency.to_string(), price);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_manager_returns_no_api_response() {
        // Unit tests never install the global PriceManager. The shim must
        // surface the same error the legacy `BITCOIN_PRICES.get` empty-map
        // path used to surface, so callers behave identically.
        let err = BitcoinPriceManager::get_price("USD").unwrap_err();
        assert!(matches!(
            err,
            MostroError::MostroInternalErr(ServiceError::NoAPIResponse)
        ));
    }
}
