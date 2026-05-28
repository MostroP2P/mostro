//! In-memory aggregated-price store with last-known-good + staleness TTL
//! (spec §6.4).
//!
//! The store is the single read surface for the rest of the daemon (from
//! Phase 1 on): the scheduler writes a fresh aggregate each tick, and
//! consumers read a currency's price through the staleness check. A
//! currency with no fresh contributors this tick simply keeps its prior
//! entry (last-known-good) — [`PriceStore::update`] only overwrites the
//! currencies present in the new aggregate.

use std::collections::HashMap;
use std::fmt;
use std::sync::RwLock;

use super::aggregate::AggregateResult;

/// A stored per-currency price plus the metadata the staleness check and
/// observability need.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AggregatedPrice {
    /// Fiat units per 1 BTC.
    pub value: f64,
    /// Unix timestamp of the last tick that produced a fresh aggregate for
    /// this currency. Anchors the staleness window.
    pub as_of: i64,
    /// How many sources contributed the value (observability / "down to one
    /// source" warnings).
    pub source_count: u8,
}

/// Read-side errors from [`PriceStore::get`]. Kept local to the price
/// module in Phase 0 (no consumers yet); Phase 1/4 map these onto
/// `MostroError` at the call sites (spec §10.2).
#[derive(Debug, PartialEq, Eq)]
pub enum PriceError {
    /// No price has ever been stored for this currency.
    NoCurrency,
    /// A price exists but is older than the configured staleness TTL.
    TooStale,
}

impl fmt::Display for PriceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PriceError::NoCurrency => write!(f, "no price available for currency"),
            PriceError::TooStale => write!(f, "price is older than the staleness window"),
        }
    }
}

impl std::error::Error for PriceError {}

/// Thread-safe map of `currency → AggregatedPrice`.
#[derive(Debug, Default)]
pub struct PriceStore {
    inner: RwLock<HashMap<String, AggregatedPrice>>,
}

impl PriceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Overwrite the currencies present in `aggregates` with `as_of = now`.
    /// Currencies **absent** from `aggregates` are left untouched, so their
    /// last-known-good value (and its older `as_of`) is preserved (spec
    /// §6.4). Currency codes are upper-cased to match read lookups.
    pub fn update(&self, aggregates: HashMap<String, AggregateResult>, now: i64) {
        if aggregates.is_empty() {
            return;
        }
        let mut w = self.inner.write().expect("price store lock poisoned");
        for (currency, agg) in aggregates {
            w.insert(
                currency.to_uppercase(),
                AggregatedPrice {
                    value: agg.value,
                    as_of: now,
                    source_count: agg.sources,
                },
            );
        }
    }

    /// Read a currency's price, enforcing the staleness window.
    ///
    /// - missing → `Err(NoCurrency)`,
    /// - `now - as_of <= max_staleness_secs` → `Ok(value)`,
    /// - otherwise → `Err(TooStale)`.
    ///
    /// The requested code is upper-cased so callers need not normalise.
    pub fn get(
        &self,
        currency: &str,
        max_staleness_secs: i64,
        now: i64,
    ) -> Result<f64, PriceError> {
        let r = self.inner.read().expect("price store lock poisoned");
        let entry = r
            .get(&currency.to_uppercase())
            .ok_or(PriceError::NoCurrency)?;
        if now.saturating_sub(entry.as_of) <= max_staleness_secs {
            Ok(entry.value)
        } else {
            Err(PriceError::TooStale)
        }
    }

    /// Snapshot of a currency's full entry (for observability / Nostr
    /// publishing in later phases). No staleness filtering.
    pub fn snapshot(&self, currency: &str) -> Option<AggregatedPrice> {
        let r = self.inner.read().expect("price store lock poisoned");
        r.get(&currency.to_uppercase()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(pairs: &[(&str, f64, u8)]) -> HashMap<String, AggregateResult> {
        pairs
            .iter()
            .map(|(c, v, s)| {
                (
                    c.to_string(),
                    AggregateResult {
                        value: *v,
                        sources: *s,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn get_fresh_within_and_past_ttl() {
        let store = PriceStore::new();
        store.update(results(&[("USD", 50_000.0, 2)]), 1_000);

        // Fresh.
        assert_eq!(store.get("USD", 1_800, 1_000).unwrap(), 50_000.0);
        // Within TTL (exactly on the boundary is still OK: `<=`).
        assert_eq!(store.get("USD", 1_800, 1_000 + 1_800).unwrap(), 50_000.0);
        // Past TTL.
        assert_eq!(
            store.get("USD", 1_800, 1_000 + 1_801).unwrap_err(),
            PriceError::TooStale
        );
    }

    #[test]
    fn get_missing_currency() {
        let store = PriceStore::new();
        assert_eq!(
            store.get("EUR", 1_800, 0).unwrap_err(),
            PriceError::NoCurrency
        );
    }

    #[test]
    fn get_is_case_insensitive() {
        let store = PriceStore::new();
        store.update(results(&[("usd", 50_000.0, 1)]), 0);
        assert_eq!(store.get("USD", 1_800, 0).unwrap(), 50_000.0);
        assert_eq!(store.get("usd", 1_800, 0).unwrap(), 50_000.0);
    }

    #[test]
    fn update_preserves_last_known_good_for_absent_currencies() {
        let store = PriceStore::new();
        store.update(
            results(&[("USD", 50_000.0, 2), ("EUR", 45_000.0, 2)]),
            1_000,
        );

        // Next tick only refreshes USD; EUR keeps its old value AND old as_of.
        store.update(results(&[("USD", 51_000.0, 2)]), 2_000);

        assert_eq!(store.snapshot("USD").unwrap().as_of, 2_000);
        assert_eq!(store.snapshot("USD").unwrap().value, 51_000.0);
        let eur = store.snapshot("EUR").unwrap();
        assert_eq!(eur.as_of, 1_000, "EUR keeps its older as_of");
        assert_eq!(eur.value, 45_000.0);

        // EUR now ages out against its stale as_of.
        assert_eq!(
            store.get("EUR", 500, 2_000).unwrap_err(),
            PriceError::TooStale
        );
    }

    #[test]
    fn empty_update_is_noop() {
        let store = PriceStore::new();
        store.update(results(&[("USD", 50_000.0, 1)]), 1_000);
        store.update(HashMap::new(), 9_999);
        // USD untouched.
        assert_eq!(store.snapshot("USD").unwrap().as_of, 1_000);
    }
}
