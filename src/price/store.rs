//! In-memory aggregated-price store with last-known-good + staleness TTL
//! (spec §6.4).
//!
//! The store is the single read surface for the rest of the daemon (from
//! Phase 1 on): the scheduler writes a fresh aggregate each tick, and
//! consumers read a currency's price through the staleness check. A
//! currency with no fresh contributors this tick simply keeps its prior
//! entry (last-known-good) — a write only touches the currencies present
//! in the new aggregate, and [`PriceStore::update_observed`] may leave even
//! one of those alone when the incoming observation is the older of the
//! two.

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
    /// Unix timestamp of when the value was **observed** — the storing
    /// tick's own clock for a directly-fetched rate, the source event's
    /// `created_at` for one relayed over Nostr (issue #860). Anchors the
    /// staleness window.
    pub as_of: i64,
    /// Unix timestamp of when **this node** last wrote the value — always
    /// the storing tick's clock, even for a relayed rate whose `as_of` is
    /// backdated to the source event. Answers "did our tick refresh it?",
    /// which is a different question from `as_of`'s "how old is this price?"
    /// — the `observe_freshness` warning measures its interval from here so
    /// a relayed event that arrives already older than one interval doesn't
    /// trip it on a healthy node (issue #860 review, PR #925).
    pub written_at: i64,
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

    /// Overwrite the currencies present in `aggregates`, stamping each with
    /// `as_of` — the time the value was **observed**, which is the tick's
    /// `now` for a directly-fetched rate but the source event's own
    /// timestamp for a relayed one (issue #860).
    ///
    /// Currencies **absent** from `aggregates` are left untouched, so their
    /// last-known-good value (and its older `as_of`) is preserved (spec
    /// §6.4). Currency codes are upper-cased to match read lookups.
    ///
    /// Every entry lands: a wall-clock write is unconditional (see
    /// [`Self::update_observed`] for why the guard is not here).
    pub fn update(&self, aggregates: HashMap<String, AggregateResult>, as_of: i64) {
        // A direct write is observed and written at the same instant.
        self.write(aggregates, as_of, as_of, false);
    }

    /// Like [`Self::update`], but for a **backdated** write: `observed_at`
    /// is a timestamp this node did not generate (a relayed event's own
    /// `created_at`), so it may predate what is already stored. `now` is
    /// this node's own clock, recorded as `written_at` so the freshness
    /// warning still measures from when we wrote the value.
    ///
    /// **Such a write never moves `as_of` backwards.** A relayed rate whose
    /// event predates a value this node fetched directly is not news, and
    /// applying it would *shorten* the serving window below what writing
    /// nothing at all would have left — refusing a currency that was
    /// perfectly servable a moment earlier. **Returns the currency codes it
    /// dropped**, so a discarded rate leaves a trace naming itself rather
    /// than a bare count nothing can be diagnosed from (review on PR #925).
    ///
    /// The guard is deliberately **not** on [`Self::update`]. A wall-clock
    /// write is this node's own authoritative observation and must land even
    /// when `now` sits behind a stamp we already hold — which a backwards
    /// clock step (NTP step after a bad-RTC boot, a resumed VM snapshot)
    /// makes possible. Guarding it there froze every currency at its
    /// pre-jump price while `get` still reported it fresh, since
    /// `now - as_of` goes negative and negative is inside any TTL.
    pub fn update_observed(
        &self,
        aggregates: HashMap<String, AggregateResult>,
        now: i64,
        observed_at: i64,
    ) -> Vec<String> {
        self.write(aggregates, now, observed_at, true)
    }

    fn write(
        &self,
        aggregates: HashMap<String, AggregateResult>,
        written_at: i64,
        as_of: i64,
        monotonic: bool,
    ) -> Vec<String> {
        let mut dropped = Vec::new();
        if aggregates.is_empty() {
            return dropped;
        }
        let mut w = self.inner.write().expect("price store lock poisoned");
        for (currency, agg) in aggregates {
            let key = currency.to_uppercase();
            if monotonic && w.get(&key).is_some_and(|prior| prior.as_of > as_of) {
                dropped.push(key);
                continue;
            }
            w.insert(
                key,
                AggregatedPrice {
                    value: agg.value,
                    as_of,
                    written_at,
                    source_count: agg.sources,
                },
            );
        }
        dropped
    }

    /// How many stored currencies [`Self::get`] would serve at `now` — the
    /// same `now - as_of <= max_staleness_secs` predicate, over **every**
    /// entry, under one read lock.
    ///
    /// This answers "what can this node serve right now", which is the only
    /// question the operator-facing tick report is actually asking. Two
    /// narrower counts were both wrong, in opposite directions:
    ///
    /// - the size of the tick's aggregate map counts a currency the tick has
    ///   just stamped **past** the TTL, since `as_of` is no longer always the
    ///   wall clock (issue #860) — it lies upward;
    /// - restricting this count to the tick's own currencies omits every
    ///   last-known-good value still inside its window, which is precisely
    ///   what a partial outage leaves behind — it lies downward.
    ///
    /// Counting *applied writes* is wrong too: a write dropped by
    /// [`Self::update_observed`]'s guard leaves a **fresher** value in place,
    /// so that currency is still servable. Only the stored entry knows.
    pub fn servable_count(&self, max_staleness_secs: i64, now: i64) -> usize {
        let r = self.inner.read().expect("price store lock poisoned");
        r.values()
            .filter(|e| now.saturating_sub(e.as_of) <= max_staleness_secs)
            .count()
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
                        // Store tests are agnostic to contributors — they
                        // exercise TTL/last-known-good semantics. Empty
                        // is fine since the store never reads this field.
                        contributors: Vec::new(),
                        nostr_anchor_dependent: false,
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

    /// The monotonicity guard, at the layer that owns it. Review on PR #925:
    /// the only coverage ran through the whole manager, and none of it
    /// asserted that the **value** is dropped along with the stamp — a
    /// backdated write must be discarded whole, not split into a new price
    /// wearing an old timestamp.
    #[test]
    fn update_never_regresses_as_of_and_drops_the_value_with_it() {
        let store = PriceStore::new();
        store.update(results(&[("USD", 50_000.0, 2)]), 2_000);

        // An older observation for the same currency is not news, and the
        // drop names itself so the trace can diagnose which rate stopped.
        // The tick clock (3_000) differs from the stored `written_at` (2_000)
        // on purpose, so a dropped write that still stamped it would show.
        assert_eq!(
            store.update_observed(results(&[("USD", 41_000.0, 1)]), 3_000, 1_000),
            vec!["USD".to_string()],
            "a backwards write must name itself as dropped"
        );

        let entry = store.snapshot("USD").unwrap();
        assert_eq!(entry.as_of, 2_000, "as_of must never move backwards");
        assert_eq!(
            entry.value, 50_000.0,
            "the value is dropped along with the stamp"
        );
        assert_eq!(entry.source_count, 2, "and so is its source count");
        assert_eq!(
            entry.written_at, 2_000,
            "a dropped write did not refresh the value, so the freshness clock stays"
        );

        // Equal stamps still apply: a re-observation at the same instant is
        // not a regression, and the guard is `>`, not `>=`.
        assert!(store
            .update_observed(results(&[("USD", 52_000.0, 3)]), 3_000, 2_000)
            .is_empty());
        // A re-observation of the same event lands too, so it stamps the tick
        // clock: a relay stuck on one event reads as refreshed every tick, and
        // is caught by the TTL rather than by the freshness warning.
        let entry = store.snapshot("USD").unwrap();
        assert_eq!(entry.value, 52_000.0);
        assert_eq!(
            entry.written_at, 3_000,
            "a landed write stamps the tick clock, re-observations included"
        );
    }

    /// A backdated write records `written_at` from the tick clock, not from
    /// the backdated `as_of` — so the freshness warning measures from when
    /// we wrote it, while the TTL keeps measuring from the observation
    /// (issue #860 review, PR #925).
    #[test]
    fn update_observed_stamps_written_at_from_the_tick_clock() {
        let store = PriceStore::new();
        // Tick at now = 5_000 stores a rate observed 900s earlier.
        store.update_observed(results(&[("ARS", 105_000_000.0, 1)]), 5_000, 4_100);

        let entry = store.snapshot("ARS").unwrap();
        assert_eq!(entry.as_of, 4_100, "the TTL clock is the observation time");
        assert_eq!(
            entry.written_at, 5_000,
            "the freshness clock is when this node wrote it"
        );
    }

    /// The monotonicity guard must not reach a wall-clock write. Found by
    /// `/code-review` on this PR: with the guard on `update` too, a
    /// backwards clock step (NTP step after a bad-RTC boot, a resumed VM
    /// snapshot) made `now` fall behind the stored `as_of`, so **every**
    /// direct write for **every** currency was silently dropped — and
    /// `get` kept serving the frozen pre-jump price as fresh, because
    /// `now - as_of` goes negative and negative is inside any TTL. An hour
    /// of a stale price presented as current, with only a `debug!` line
    /// blaming a stale observation.
    #[test]
    fn a_backwards_clock_step_does_not_freeze_direct_writes() {
        let store = PriceStore::new();
        store.update(results(&[("USD", 50_000.0, 2)]), 10_000);

        // The clock steps back an hour. This write is authoritative.
        store.update(results(&[("USD", 60_000.0, 2)]), 6_400);

        let entry = store.snapshot("USD").unwrap();
        assert_eq!(entry.value, 60_000.0, "the new price must be served");
        assert_eq!(entry.as_of, 6_400, "and stamped at the clock we now have");
        assert_eq!(store.get("USD", 1_800, 6_400).unwrap(), 60_000.0);
    }

    /// `servable_count` must agree with [`PriceStore::get`] entry by entry,
    /// over the whole store — including the entry a tick's aggregate map
    /// cannot see (a last-known-good value from an earlier tick) and the one
    /// it would wrongly include (an entry stamped past the TTL).
    #[test]
    fn servable_count_agrees_with_get() {
        let store = PriceStore::new();
        store.update(results(&[("USD", 50_000.0, 2)]), 1_000);
        // Inside a 500s TTL at now = 2_000; USD, at 1_000, is not.
        store.update(results(&[("ARS", 105_000_000.0, 1)]), 1_600);
        store.update(results(&[("EUR", 45_000.0, 1)]), 1_700);

        assert_eq!(
            store.servable_count(500, 2_000),
            2,
            "ARS and EUR are inside the window; USD aged out"
        );
        // Entry by entry, the same predicate as `get`.
        assert!(store.get("ARS", 500, 2_000).is_ok());
        assert!(store.get("EUR", 500, 2_000).is_ok());
        assert_eq!(
            store.get("USD", 500, 2_000).unwrap_err(),
            PriceError::TooStale
        );

        // Widening the window brings the aged-out entry back into the count.
        assert_eq!(store.servable_count(1_500, 2_000), 3);
        // An empty store counts zero rather than panicking on the lock.
        assert_eq!(PriceStore::new().servable_count(1_800, 2_000), 0);
    }
}
