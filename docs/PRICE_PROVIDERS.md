# Multi-Source Price Providers — Implementation Spec

> Implementation guide for removing the single-source (Yadio) price
> dependency. This document is the single source of truth as the feature
> is rolled out across several PRs. Each phase below maps to one **small,
> atomic PR** that can be reviewed, tested, and released independently.
>
> **Status of design decisions:** the aggregation method, per-currency
> averaging, staleness policy, and live-path unification were settled with
> the maintainer (see §4). One item is still flagged for confirmation: the
> El Toque fiat-cross modelling in §11.3.

## 1. Goal

Replace the hard dependency on the **Yadio** API for BTC/fiat prices with
a **multi-source price module**. The module:

- Queries **several independent price APIs** behind a single common
  interface (one `.rs` file per API), so `mostrod` carries **no
  per-API code** outside each provider's adapter.
- **Aggregates per currency** across every API that reports it and is
  currently healthy, using a robust median + outlier-guard (see §6).
- **Degrades gracefully**: a down API is simply ignored for the current
  aggregation; a currency keeps serving its last-known-good value until a
  staleness TTL elapses.
- Treats specialised sources naturally: a source that only reports CUP/MLC
  (El Toque) only ever contributes to CUP/MLC; a source with no CUP/MLC
  (CoinGecko) only contributes to the currencies it does report.

Today Yadio is a **single point of failure** for 100+ fiat currencies:
when `api.yadio.io` is down, every market-priced order across every
currency is stuck on stale data. This module removes that.

## 2. Current state (what this replaces)

Two Yadio-only integration points exist today:

| Path | Code | Trigger | Endpoint |
|------|------|---------|----------|
| **Cached** | `BitcoinPriceManager` (`src/bitcoin_price.rs`) | scheduler `job_update_bitcoin_prices` every `exchange_rates_update_interval_seconds` | Yadio `GET /exrates/BTC` → `{ "BTC": { "USD": 50000, … } }` |
| **Live** | `get_market_quote` (`src/util.rs`) | every market-priced take via `get_market_amount_and_fee` | Yadio `GET /convert/{amt}/{ccy}/BTC` |

- The cached path stores a `HashMap<String, f64>` (currency → fiat-per-BTC)
  in a static `BITCOIN_PRICES`, read by `get_bitcoin_price()`
  (`util.rs`), used at order creation (`app/order.rs`, when `amount == 0`).
- The cached path also publishes rates to Nostr as a NIP-33 kind-30078
  event with a `source: yadio` tag (see `docs/NOSTR_EXCHANGE_RATES.md`).
- The live path does its own retry loop (`retries_yadio_request`, 4
  tries) and is the second place Yadio is hard-wired.

Both are absorbed into the new module; see Phases 1 and 4.

## 3. Guiding principles

1. **One interface, many providers.** Every API is a `PriceProvider`
   implementation in its own file under `src/price/providers/`. Adding a
   new API is a new adapter file, one registry line, and one config
   entry — never a change to the aggregation core, the scheduler, or any
   handler. The full checklist is §5.4.
2. **Per-currency, source-agnostic aggregation.** The aggregate for a
   currency is computed only from the providers that report it and are
   healthy *right now*. There is no global "primary" provider; Yadio is
   just one source among several.
3. **Never let one bad source move the market.** Aggregation is
   outlier-resistant (§6). A single API returning a corrupt or stale value
   must not drag the result.
4. **Degrade, don't fail.** A down API is skipped for the current tick. A
   currency with no fresh source falls back to its last-known-good value
   until a configurable staleness TTL; only then is it refused.
5. **Opt-in per provider; safe defaults.** A node runs whatever subset of
   providers it configures. The default config reproduces today's
   behaviour closely (Yadio enabled) plus at least one keyless backup, so
   upgrading is strictly more resilient with zero required config.
6. **Secrets stay secret.** Paid providers take an API token from config
   only; tokens are never logged, never published to Nostr, never put on
   an audit event.
7. **Tests accompany every phase.** Aggregation math is pure and unit
   tested exhaustively; provider adapters are tested against captured
   sample payloads. `cargo test`, `cargo clippy --all-targets
   --all-features`, and `cargo fmt` stay green.

## 4. Settled design decisions

Confirmed with the maintainer before writing this spec:

- **Aggregation = median + outlier guard** (§6.2). Not a plain mean.
- **Per-currency averaging, no authoritative override.** All healthy
  sources that report a currency are combined equally; e.g. CUP combines
  Yadio + El Toque by aggregation, and if one is down the other is used.
  No per-currency "this source wins" mechanism in v1.
- **Staleness = last-known-good + TTL** (§6.4). Serve the last value per
  currency up to `max_price_staleness_seconds`; past that, refuse
  market-priced operations for that currency with a clear error.
- **Unify the live path onto the cache** (§9, Phase 4).
  `get_market_quote` reads the aggregated cache instead of calling Yadio
  `/convert` per take. No per-take HTTP, one multi-source path.

## 5. Architecture overview

### 5.1 Module layout

```
src/price/
  mod.rs            -- PriceManager: public API + scheduler entry point
  provider.rs       -- PriceProvider trait, Quote, ProviderId, health/circuit-breaker
  aggregate.rs      -- pure aggregation (anchor resolution, median+outlier, staleness)
  store.rs          -- in-memory aggregated-price store (RwLock<HashMap<..>>)
  config.rs         -- typed [price] config + per-provider sub-config
  providers/
    mod.rs
    yadio.rs        -- direct BTC quoter, 120+ currencies incl. CUP/MLC
    coingecko.rs    -- direct BTC quoter, many currencies, no CUP/MLC
    eltoque.rs      -- fiat-cross quoter, CUP/MLC only (needs USD anchor, §11.3)
```

`src/bitcoin_price.rs` is retired at the end of the rollout (Phase 5);
its public surface (`BitcoinPriceManager::get_price`, `update_prices`) is
re-exported as thin shims during the transition so consumers migrate one
at a time.

### 5.2 The provider interface

```rust
/// A single currency quote from a provider.
pub enum Quote {
    /// Fiat units per 1 BTC (Yadio, CoinGecko). Directly aggregatable.
    PerBtc(f64),
    /// `value` units of this currency per 1 unit of `base` currency
    /// (El Toque: CUP per USD). Resolved to a per-BTC figure by
    /// multiplying by the aggregated `base`/BTC price (§6.3). Lets a
    /// fiat-to-fiat source contribute without itself knowing the BTC price.
    PerBase { base: String, value: f64 },
}

/// Result of one provider poll: currency code -> quote.
pub type ProviderQuotes = std::collections::HashMap<String, Quote>;

#[async_trait::async_trait]
pub trait PriceProvider: Send + Sync {
    /// Stable identifier used in logs, config keys, health tracking, and
    /// the Nostr `source` metadata.
    fn id(&self) -> ProviderId;

    /// Fetch the latest quotes. Returns only the currencies this provider
    /// actually reports; a network/parse failure is an `Err` (the whole
    /// provider is skipped for this tick, never a partial map with bogus
    /// values).
    async fn fetch(&self, http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError>;
}
```

- `mostrod` only ever holds `Vec<Box<dyn PriceProvider>>`. The aggregation
  core and scheduler are provider-agnostic.
- A provider returning `Quote::PerBtc` is a **direct** quoter; a provider
  returning `Quote::PerBase` is a **fiat-cross** quoter (§6.3, §11.3).
- Partial coverage is the norm: each provider returns only what it has.
  El Toque returns `{ CUP, MLC }`; CoinGecko returns its supported set
  (no CUP/MLC); Yadio returns 120+. The aggregator unions them per
  currency.

### 5.3 The aggregation pipeline (one scheduler tick)

```
                 ┌─ yadio.fetch()    ─┐  (PerBtc)
poll all healthy ├─ coingecko.fetch()─┤  (PerBtc)   each with per-provider
providers, in    └─ eltoque.fetch()  ─┘  (PerBase)  timeout + circuit breaker
parallel
        │
        ▼
 1. collect PerBtc quotes  → per-currency candidate lists (the "anchors")
 2. resolve PerBase quotes → currency/BTC = value × aggregate(base/BTC)   (§6.3)
 3. aggregate per currency → median + outlier guard / mean / single       (§6.2)
 4. write store: { currency -> AggregatedPrice { value, as_of: now, sources } }
    - currencies with zero fresh contributors this tick keep their prior
      AggregatedPrice (last-known-good, old `as_of`).
        │
        ▼
 reads: PriceManager::get_price(ccy) -> staleness-checked value           (§6.4)
```

### 5.4 Adding a new provider (the extension contract)

This is the payoff of the whole design: a new API is **one small adapter
file + one enum variant + one registry line + one config block + one
fixture test**. You touch *only* those; the aggregation core, the store,
the scheduler, and every order handler stay untouched.

**Step 1 — write the adapter.** `src/price/providers/<id>.rs`,
implementing `PriceProvider`. Map only the currencies the API actually
reports, and pick the quote flavour:

- a **direct** BTC quoter returns `Quote::PerBtc(fiat_per_btc)`;
- a **fiat-cross** quoter returns `Quote::PerBase { base, value }`
  (resolved against the aggregated `base`/BTC anchor, §6.3).

Two normalisation rules every adapter must follow (§6.6):

- **Canonicalise currency codes to uppercase ISO-4217.** Providers
  disagree on casing — `currency-api` ships lowercase (`"usd"`),
  Yadio/Blockchain ship uppercase (`"USD"`). The aggregator keys on the
  code, so an un-normalised adapter would silently fail to combine its
  values with everyone else's.
- **Emit one mid-market price per currency.** Ignore any bid/ask/spread
  fields (e.g. Blockchain's `buy`/`sell`; use `last`). Mostro prices at
  mid-market and applies its own premium/fee separately — providers must
  not bake in a spread (§6.6, §11.6).

```rust
// src/price/providers/myapi.rs
use async_trait::async_trait;
use crate::price::provider::{
    PriceProvider, ProviderConfig, ProviderError, ProviderId, ProviderQuotes, Quote,
};

pub struct MyApiProvider {
    url: String,
    // token / api_key pulled from ProviderConfig if the API needs one
}

impl MyApiProvider {
    pub fn new(cfg: &ProviderConfig) -> Self {
        Self { url: cfg.url.clone() }
    }
}

#[async_trait]
impl PriceProvider for MyApiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::MyApi
    }

    async fn fetch(&self, http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
        // one HTTP call, parse, map to currency -> Quote. That is all.
        let body: MyApiResponse = http
            .get(format!("{}/rates", self.url))
            .send()
            .await?
            .json()
            .await?;
        Ok(body
            .rates
            .into_iter()
            .map(|(ccy, price)| (ccy, Quote::PerBtc(price)))
            .collect())
    }
}
```

**Step 2 — declare it.** Add a `ProviderId::MyApi` variant and
`pub mod myapi;` in `src/price/providers/mod.rs`.

**Step 3 — register it.** Add exactly one arm to the registry builder —
the *single* designated extension point in the codebase:

```rust
// src/price/mod.rs
fn build_provider(id: ProviderId, cfg: &ProviderConfig) -> Box<dyn PriceProvider> {
    match id {
        ProviderId::Yadio     => Box::new(yadio::YadioProvider::new(cfg)),
        ProviderId::CoinGecko => Box::new(coingecko::CoinGeckoProvider::new(cfg)),
        ProviderId::ElToque   => Box::new(eltoque::ElToqueProvider::new(cfg)),
        ProviderId::MyApi     => Box::new(myapi::MyApiProvider::new(cfg)), // <- the one line
    }
}
```

**Step 4 — configure it.** Add a `[price.providers.myapi]` block to
`settings.tpl.toml` with `enabled`, `url`, and any `token`/`api_key`.
If a secret is required, add it to the startup validation (§7) so an
enabled-but-unconfigured provider fails fast.

**Step 5 — test it.** Commit a captured real response as a fixture and
add one parse test (`fetch`-maps-fixture → expected quotes). The
aggregation needs **no** new tests — it is provider-agnostic and already
covered by Phase 0.

**What you never touch:** `aggregate.rs`, `store.rs`, the scheduler tick,
`get_bitcoin_price` / `get_market_quote`, or any order handler. That
invariant is what makes provider count a config concern, not an
engineering project — and it is locked down by principle §3.1 and the
acceptance tests of each provider phase (1, 2, 3), each of which adds a
provider **without** modifying the core.

## 6. Aggregation algorithm (normative)

### 6.1 Inputs

Per tick, for each healthy provider, a `ProviderQuotes` map (or an error →
the provider contributes nothing this tick).

### 6.2 Per-currency combine

Given the list `xs` of candidate per-BTC prices for a currency (after
PerBase resolution, §6.3):

```text
n = len(xs)
n == 0 -> no fresh value (fall through to last-known-good, §6.4)
n == 1 -> xs[0]
n == 2 -> mean(xs)
n >= 3 -> m   = median(xs)
          kept = [x in xs if |x - m| / m <= outlier_threshold_pct/100]
          result = mean(kept)          # `kept` always contains m, so non-empty
```

`outlier_threshold_pct` defaults to **5.0**. The median anchors the
"truth"; values too far from it are discarded before the mean, so one
corrupt/stale source cannot move the result while genuine small spreads
between honest sources are still averaged in.

### 6.3 PerBase resolution (fiat-cross sources)

A `Quote::PerBase { base, value }` for currency `C` resolves to a per-BTC
candidate **only if** the aggregate per-BTC price for `base` is available
this tick (from the direct quoters in step 1):

```text
candidate(C) = value × aggregate_per_btc(base)
```

Worked example (El Toque, base = USD):

```text
El Toque:  CUP per USD = 400      -> Quote::PerBase { base: "USD", value: 400 }
Aggregate: USD per BTC = 50_000   (from Yadio + CoinGecko)
=> CUP per BTC candidate = 400 × 50_000 = 20_000_000
```

If `base`'s per-BTC anchor is unavailable (all direct quoters down), the
PerBase quote is dropped for this tick and the currency falls back to
last-known-good. The dependency is documented so operators understand
El Toque's CUP/MLC need at least one direct USD source to be live.

> The El Toque adapter performs any internal cross math (e.g. deriving
> *MLC per USD* from its CUP-denominated payload) **inside**
> `providers/eltoque.rs`, emitting clean `PerBase { base: "USD", … }`
> quotes. The aggregator stays generic. See §11.3.

### 6.4 Staleness (last-known-good + TTL)

Each currency's stored `AggregatedPrice` carries `as_of` = the timestamp
of the last tick that produced a fresh aggregate for it.

- A tick that yields a fresh value overwrites the entry with `as_of = now`.
- A tick with zero contributors for a currency leaves the prior entry
  untouched (old `as_of`).
- `PriceManager::get_price(ccy)`:
  - entry missing → `Err(NoCurrency)`.
  - `now - as_of <= max_price_staleness_seconds` → `Ok(value)` (a `warn!`
    is logged once the value is older than one update interval).
  - else → `Err(PriceTooStale)` (§10.2). Market-priced create/take for
    that currency is refused with a clear message.

`max_price_staleness_seconds` defaults to **1800** (30 min) — long enough
to ride out short API outages, short enough that nobody trades on an
hours-old quote.

### 6.5 Per-provider health / circuit breaker

Per provider, track consecutive failures. After `provider_failure_threshold`
(default 3) consecutive failures, skip the provider for a cooldown that
backs off exponentially from `provider_failure_cooldown_seconds` (default
120) up to a cap (default 1800). A success resets the counter. This keeps
a hard-down API from slowing every tick (each poll is also bounded by
`provider_timeout_seconds`, default 10) and from spamming error logs.

### 6.6 Currency normalisation, scoping, and mid-market

These rules sit between the adapters and §6.2's combine, and are what make
heterogeneous providers safely comparable.

- **Code canonicalisation.** All currency codes are upper-cased before
  they reach the combine step (adapters do this; the read path upper-cases
  the requested code too). Without this, `currency-api`'s `"usd"` and
  Yadio's `"USD"` would form two separate, single-source aggregates.
- **Fiat allowlist.** Providers return junk for our purposes: `currency-api`
  ships **324** entries including crypto (`eth`, `bnb`, `ada`) and
  non-ISO codes. The aggregator restricts to the node's known fiat set
  (the currencies Mostro already validates for orders); everything else is
  dropped before aggregation and before the Nostr publish, keeping the
  store and the kind-30078 event lean.
- **Per-provider currency scoping (`only` / `except`).** Some providers
  report a currency on a **different market** than others, and averaging
  across markets is wrong — not merely noisy. The motivating case is
  **CUP**: `currency-api` reports CUP at the **official** rate (~24
  CUP/USD), while Yadio and El Toque track the **informal** rate (~400
  CUP/USD) — a ~16× gap. Config can restrict which currencies a provider
  may contribute (`except = ["CUP","MLC"]` on `currency-api`, or
  `only = ["CUP","MLC"]` on El Toque). Scoping is applied **before**
  combine, so a mis-marked source never enters the median at all.
  - The §6.2 outlier guard is the **safety net** for *accidental*
    divergence with ≥3 honest sources (the official-rate value is
    discarded as an outlier), but it does **not** help with only two
    sources (`mean` of official + informal = garbage). Scoping is the
    deterministic fix; the outlier guard backstops it. Note this refines
    the "no authoritative override" decision (§4): scoping restricts a
    source's *coverage*, it does not pick a winner among legitimate
    same-market contributors.
- **Mid-market only.** Adapters discard bid/ask; Mostro never applies an
  exchange spread (contrast BTCPay, §11.6). The order premium/fee is the
  only markup, applied downstream in `get_market_quote` / `get_fee`.

## 7. Configuration surface (final shape)

New `[price]` section. Missing section ≡ "Yadio only, today's behaviour"
(see §10.1 migration).

```toml
[price]
# How often to poll providers and recompute the aggregate.
update_interval_seconds = 300
# Serve a currency's last-known-good value up to this age; then refuse.
max_price_staleness_seconds = 1800
# Discard a source whose value deviates more than this % from the median
# (only applies with >= 3 sources for a currency).
outlier_threshold_pct = 5.0
# Per-provider request timeout.
provider_timeout_seconds = 10
# Circuit breaker.
provider_failure_threshold = 3
provider_failure_cooldown_seconds = 120
# Publish the aggregated rates to Nostr (kind 30078). Replaces
# publish_exchange_rates_to_nostr.
publish_to_nostr = true

[price.providers.yadio]
enabled = true
url = "https://api.yadio.io"

[price.providers.coingecko]
enabled = true
url = "https://api.coingecko.com/api/v3"
# api_key = "CG-xxxx"   # optional demo/pro key; raises rate limits

# Keyless, CDN-hosted, 300+ currencies incl. CUP (OFFICIAL rate) — so CUP
# is excluded to avoid mixing it with the informal-market sources (§6.6).
[price.providers.currency_api]
enabled = true
url = "https://currency-api.pages.dev/v1"
# Optional ordered mirrors, tried in sequence if `url` fails (§7).
fallback_urls = ["https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api@latest/v1"]
except = ["CUP", "MLC"]

# Keyless, 28 major fiats, no CUP/MLC.
[price.providers.blockchain]
enabled = true
url = "https://blockchain.info"

[price.providers.eltoque]
enabled = false          # opt-in: requires a token
url = "https://tasas.eltoque.com"
# token = "xxxx"         # REQUIRED when enabled; provider refuses to start otherwise
only = ["CUP", "MLC"]    # El Toque is only meaningful for these (§6.6)
```

- Each `[price.providers.<id>]` sub-table is deserialized into a generic
  `ProviderConfig { enabled, url, fallback_urls?, api_key?, token?, only?,
  except? }`. Adding a provider adds a sub-table; the loader maps known
  ids to their adapter (§5.4).
  - `fallback_urls`: ordered mirrors tried when `url` fails this tick
    (e.g. `currency-api`'s jsdelivr mirror) — provider-level resilience
    on top of the multi-provider resilience.
  - `only` / `except`: per-provider currency allow/deny applied before
    aggregation (§6.6). `only` and `except` are mutually exclusive.
- Validation at startup: an enabled provider missing a required secret
  (El Toque without `token`) fails fast with a descriptive error rather
  than silently producing no quotes; `only` ∩ `except` set on the same
  provider is rejected.

## 8. Phase overview

| Phase | PR scope | Depends on | Status |
|------:|----------|------------|--------|
| 0 | Foundation: `PriceProvider` trait, `Quote`, aggregation core (pure), store, `[price]` config types | — | pending |
| 1 | Yadio provider + registry + scheduler wiring (single-source parity); `get_bitcoin_price` reads new store | 0 | pending |
| 2 | Direct backup quoters (CoinGecko, currency-api, Blockchain.com) → real multi-source aggregation; per-provider health/circuit-breaker; currency normalisation + fiat allowlist + per-provider scoping | 1 | pending |
| 3 | El Toque provider (fiat-cross CUP/MLC) via PerBase anchor resolution | 2 | pending |
| 4 | Unify `get_market_quote` onto the cache; staleness TTL enforcement (`PriceTooStale`) at create/take | 2 | pending |
| 5 | Nostr aggregated publishing + token/paid-provider support polish + info-event exposure + retire `bitcoin_price.rs` + ops docs | 3, 4 | pending |

Phases 3 and 4 both depend on Phase 2 and can land in either order.

---

## 9. Phase details

### Phase 0 — Foundation (pure, no wiring)

**Scope**
- `src/price/provider.rs`: `PriceProvider` trait, `Quote`, `ProviderId`,
  `ProviderError`, `ProviderConfig`, health/circuit-breaker state type.
- `src/price/aggregate.rs`: pure functions —
  `combine(xs, outlier_pct) -> Option<f64>` (§6.2),
  `resolve_per_base(quotes, anchors) -> per_currency_candidates` (§6.3),
  `aggregate_tick(provider_results, cfg) -> HashMap<String, f64>` (steps
  1–3 of §5.3). No I/O, no globals.
- `src/price/store.rs`: `AggregatedPrice { value, as_of, source_count }`,
  the `RwLock<HashMap<String, AggregatedPrice>>` store, and
  staleness-checked `get` (§6.4).
- `src/price/config.rs`: `PriceSettings` + `ProviderConfig` serde types
  with defaults from §7. Add `Option<PriceSettings>` to `Settings` with a
  `Settings::get_price()` accessor.

**Non-goals:** no HTTP, no scheduler change, no consumer change.

**Acceptance / tests**
- `combine`: 0/1/2/≥3 sources; outlier discarded at the boundary; all-equal;
  NaN/inf/≤0 rejected as inputs.
- `resolve_per_base`: resolves with anchor present; drops when anchor
  missing; the El Toque worked example (§6.3).
- `aggregate_tick`: union of partial-coverage providers; a provider error
  contributes nothing; CUP from {Yadio,ElToque}, EUR from
  {Yadio,CoinGecko,ElToque-absent}.
- staleness `get`: fresh / within-TTL / past-TTL / missing.

### Phase 1 — Yadio provider + registry (single-source parity)

**Scope**
- `src/price/providers/yadio.rs`: `YadioProvider` implementing
  `PriceProvider` via `GET {url}/exrates/BTC`, mapping the
  `{ "BTC": { ccy: price } }` body to `PerBtc` quotes.
- `src/price/mod.rs`: `PriceManager` building the provider registry from
  `[price]`, a `update_all(&self)` tick (poll → aggregate → store), and
  `get_price(ccy)` (staleness check **logged but not enforced** yet — see
  Phase 4, to preserve current "never refuse" behaviour during rollout).
- Scheduler: `job_update_bitcoin_prices` calls `PriceManager::update_all`.
- `get_bitcoin_price` (`util.rs`) reads `PriceManager` instead of
  `BitcoinPriceManager`. Keep a `BitcoinPriceManager::get_price` shim
  delegating to `PriceManager`.
- Nostr publishing keeps working unchanged (still effectively one source);
  `source` tag becomes the contributing-source list (here, `["yadio"]`).

**Acceptance / tests**
- With only Yadio enabled, `get_bitcoin_price` returns the same values as
  today for the captured sample payload.
- Yadio down for a tick → store keeps prior values; no panic.
- `enabled = false` on Yadio with no other provider → empty store; reads
  return `NoCurrency` (logged), matching "no data yet" today.

### Phase 2 — Direct backup quoters + multi-source aggregation

Adds the keyless direct backups so the system is genuinely multi-source.
Each is a `PerBtc` adapter (§5.4) — they exercise the same contract, so
they can land in one PR or be split per provider.

**Scope**
- `src/price/providers/coingecko.rs`: via
  `GET {url}/simple/price?ids=bitcoin&vs_currencies=<list>`, `PerBtc`,
  optional `api_key`. No CUP/MLC.
- `src/price/providers/currency_api.rs`: via
  `GET {url}/currencies/btc.min.json`, lowercase codes upper-cased,
  `PerBtc`. Wide coverage incl. CUP at the **official** rate → ships
  `except = ["CUP","MLC"]` (§6.6). Uses `fallback_urls` (jsdelivr mirror).
- `src/price/providers/blockchain.rs`: via
  `GET {url}/ticker`, takes `last` (mid-market), `PerBtc`. 28 majors.
- Wire the circuit breaker + per-provider timeout (§6.5) into
  `update_all` (parallel `fetch` with `tokio`).
- Implement the §6.6 pipeline glue: code upper-casing, fiat allowlist,
  per-provider `only`/`except` scoping.

**Acceptance / tests**
- EUR/USD/JPY aggregate = median+outlier across all live direct quoters.
- Lowercase `currency-api` codes combine with uppercase Yadio codes (the
  normalisation test — would silently fail without §6.6).
- `currency-api`'s official CUP is **scoped out**, so it never enters the
  CUP aggregate; with a synthetic 3rd informal source it would also be
  rejected by the outlier guard (both layers tested).
- non-fiat codes (`eth`, `bnb`) from `currency-api` are dropped by the
  allowlist.
- One provider returns a wild outlier with ≥3 sources → discarded.
- A provider down → currencies fall back to the remaining sources; a
  provider's `fallback_urls` is tried before the provider is marked failed.
- Circuit breaker opens after N failures and closes after cooldown.

### Phase 3 — El Toque provider (fiat-cross CUP/MLC)

**Scope**
- `src/price/providers/eltoque.rs`: `ElToqueProvider` via the El Toque
  tasas API (Bearer `token`), emitting `PerBase { base: "USD", value }`
  for CUP and MLC (internal cross math per §11.3).
- No aggregation-core change — Phase 0's PerBase resolution already
  handles it.

**Acceptance / tests**
- CUP aggregate = combine(Yadio CUP/BTC, ElToque-resolved CUP/BTC).
- El Toque up, Yadio CUP down → CUP from El Toque only.
- Yadio up, El Toque down → CUP from Yadio only.
- **Anchor dependency:** all direct USD quoters down → El Toque CUP/MLC
  drop to last-known-good (resolution impossible without a USD anchor).
- El Toque enabled without `token` → startup error.

### Phase 4 — Unify the live path + enforce staleness

**Scope**
- Rewrite `get_market_quote` (`util.rs`) to compute
  `sats = (fiat_amount / aggregate_btc_price(ccy)) × 1e8` from the cache,
  applying premium as today. Remove `retries_yadio_request` and the
  per-take Yadio `/convert` call.
- Turn on staleness enforcement (§6.4): `get_price` / `get_market_quote`
  return `PriceTooStale` past the TTL. Order create (`app/order.rs`) and
  market-priced takes surface it as a clean `CantDo`/error to the user
  instead of pricing on stale data.

**Acceptance / tests**
- `get_market_quote` parity vs the old `/convert` math for a known price.
- Past-TTL currency → create/take refused with `PriceTooStale`; other
  currencies unaffected.
- No HTTP call happens during a take (cache read only).

### Phase 5 — Nostr publishing, paid providers, exposure, cleanup

**Scope**
- Publish the **aggregated** map to Nostr (kind 30078). `source` tag
  carries the contributing provider ids; optionally a per-currency
  source-count tag. Update `docs/NOSTR_EXCHANGE_RATES.md`.
- Reference implementation + docs for a **token/paid** provider config
  (the `ProviderConfig.token`/`api_key` plumbing already exists from
  Phase 0; this validates the secret-handling end to end).
- Optionally surface bond-style policy on the Mostro info event: which
  providers are enabled (ids only, never secrets).
- Retire `src/bitcoin_price.rs` once all consumers read `PriceManager`.
- Operator docs: `docs/LIGHTNING_OPS.md` / a price-ops runbook (reading
  health logs, adding a provider, rotating a token).

---

## 10. Cross-cutting concerns

### 10.1 Backward compatibility

- **Config migration.** When `[price]` is absent, synthesise a
  default config: a single `yadio` provider using the legacy
  `bitcoin_price_api_url`, with `update_interval_seconds` =
  `exchange_rates_update_interval_seconds` and `publish_to_nostr` =
  `publish_exchange_rates_to_nostr`. So existing `settings.toml` files
  keep working byte-for-byte; the legacy keys are honoured and marked
  deprecated in the template. Default *new* config also enables the
  keyless backups — CoinGecko, `currency-api`, and Blockchain.com — so a
  fresh node is multi-source out of the box without any signup. El Toque
  stays opt-in (needs a token). With these four, USD/EUR/major pairs have
  3–4 sources (full median + outlier protection), and CUP/MLC have Yadio
  (informal) plus optionally El Toque, with `currency-api`'s official-rate
  CUP scoped out (§6.6).
- **Consumer surface.** `get_bitcoin_price` keeps its signature;
  `BitcoinPriceManager::get_price` becomes a shim until Phase 5.
- **Behaviour during rollout.** Staleness is logged-only until Phase 4,
  so Phases 1–3 never refuse an order that would have priced today.

### 10.2 mostro-core changes

- A new error for refused stale prices is likely needed:
  `ServiceError::PriceTooStale` (internal) and/or a
  `CantDoReason::PriceTooStale` (user-facing) in `mostro-core`. This is a
  cross-repo, serde-additive change pinned by version, on the same
  cadence as other protocol additions. Until it lands, Phase 4 can reuse
  `ServiceError::NoAPIResponse` and swap later. Confirm before Phase 4.

### 10.3 Security

- Tokens/keys come only from config and live only in the provider adapter.
  They must never appear in logs (`tracing`), the Nostr event, the info
  event, or error messages. A redaction test asserts a provider's `Debug`
  and any logged error omit the secret.

### 10.4 Observability

- `tracing` per tick: per-provider outcome (ok/{n currencies} | skipped:
  cooldown | error), and per-currency `source_count`. A currency dropping
  to a single source, or to last-known-good, logs at `warn`.
- No Prometheus wiring until real traffic justifies it.

### 10.5 Testing discipline

- Aggregation (`aggregate.rs`) is pure → exhaustive unit tests, the
  numeric heart of the feature.
- Provider adapters tested against **captured real payloads** committed as
  fixtures (so a provider changing its JSON shape is caught), parsing
  offline — no network in tests.
- A `MockProvider` (configurable quotes / forced errors / latency) drives
  end-to-end aggregation, circuit-breaker, and staleness tests without
  HTTP.

---

## 11. Appendix — provider notes

### 11.1 Yadio (direct, 120+ currencies incl. CUP/MLC)
- `GET /exrates/BTC` → `{ "BTC": { "USD": 50000, "CUP": 20000000, … } }`.
  Each value is fiat-per-BTC → `Quote::PerBtc`.
- The widest source; the practical anchor for USD/BTC when CoinGecko is
  down.

### 11.2 CoinGecko (direct, many currencies, NO CUP/MLC)
- `GET /simple/price?ids=bitcoin&vs_currencies=usd,eur,…` →
  `{ "bitcoin": { "usd": 50000, … } }` → `Quote::PerBtc`.
- Keyless tier is rate-limited; optional demo/pro `api_key` raises limits.
- Does not list CUP/MLC, so it only ever contributes to the currencies it
  reports — exactly the desired behaviour, no special casing.

### 11.3 El Toque (fiat-cross, CUP/MLC only) — ⚠ confirm

El Toque publishes the **informal Cuban market rate** as **CUP per
foreign unit** (e.g. CUP per USD, CUP per EUR, CUP per MLC) — it is **not
a BTC price source**. Therefore:

- Its quotes are `Quote::PerBase { base: "USD", value }`, resolved against
  the aggregated USD/BTC anchor (§6.3). CUP/MLC require **at least one
  live direct USD source** (Yadio or CoinGecko).
- The adapter derives `MLC per USD` from El Toque's CUP-denominated
  payload internally:
  `MLC_per_USD = cup_per_usd / cup_per_mlc`, then emits
  `PerBase { base: "USD", value: MLC_per_USD }` for MLC and
  `PerBase { base: "USD", value: cup_per_usd }` for CUP.
- Requires a Bearer **token** (free registration); enabled-without-token
  is a startup error.

**To confirm with the maintainer:**
1. Does the El Toque plan you intend to use expose CUP, MLC, **and** a USD
   (and/or EUR) cross in one call? The adapter math above assumes
   `cup_per_usd` and `cup_per_mlc` are both present.
2. Should EUR be a second anchor fallback for CUP when USD/BTC is
   momentarily unavailable but EUR/BTC is live? (Cheap to add; keeps
   CUP/MLC alive in more outage shapes.)
3. Confirm El Toque's CUP and Yadio's CUP track the **same** (informal)
   market, so averaging them is apples-to-apples. **Concrete evidence
   this matters:** `currency-api` (§11.5) reports CUP at the *official*
   rate (~24 CUP/USD) vs the informal ~400 CUP/USD of Yadio/El Toque — a
   ~16× gap. That is exactly why §6.6 adds per-provider currency scoping
   (`currency-api` ships with `except = ["CUP","MLC"]`). Please confirm
   Yadio's CUP is the informal rate (it has been historically); if Yadio
   ever switched to official, we would scope its CUP out too and lean on
   El Toque.

### 11.4 Blockchain.com (direct, 28 major fiats, NO CUP/MLC)
- `GET https://blockchain.info/ticker` →
  `{ "USD": { "15m":76273, "last":76273, "buy":…, "sell":…, "symbol":"USD" }, … }`.
  Uppercase codes. The adapter takes **`last`** (mid-market) → `PerBtc`,
  discarding `buy`/`sell` (§6.6 mid-market rule).
- Keyless. Only ~28 major currencies, so it is a redundancy anchor for
  USD/EUR/GBP/JPY/etc., not a long-tail source.

### 11.5 currency-api / fawazahmed0 (direct, 300+ currencies incl. CUP)
- `GET {url}/currencies/btc.min.json` →
  `{ "date":"…", "btc": { "usd":77817.3, "cup":1867119.0, … } }`.
  **Lowercase** codes (adapter upper-cases them, §6.6); values are
  fiat-per-BTC → `PerBtc`.
- Keyless and **CDN-hosted** (Cloudflare Pages, with a jsdelivr mirror —
  configure both via `fallback_urls`), so it is one of the most reliable
  backups available; an excellent default.
- **324 entries including crypto** (`eth`, `bnb`, …) → relies on the §6.6
  fiat allowlist to drop non-fiat codes.
- **CUP is the OFFICIAL rate** (~24 CUP/USD), a different market from the
  informal sources → shipped with `except = ["CUP","MLC"]` (§6.6). It
  *does* strengthen the long tail of legitimately-single-market
  currencies that Yadio also lists.

### 11.6 Prior art — BTCPayServer (design rationale)

BTCPayServer solves the same problem; contrasting choices clarify ours:

- **Provider abstraction.** BTCPay's `IRateProvider` + `RateProviderFactory`
  is the same shape as our `PriceProvider` trait + registry (§5) — strong
  validation that the abstraction is right.
- **Background fetch + cache.** BTCPay wraps providers in a
  `BackgroundFetcherRateProvider` refreshing ~every minute, decoupling
  fetch from use. That is exactly our scheduler-poll → store → read model
  with a staleness TTL (§6.4).
- **Rate-rule DSL vs fixed aggregation.** BTCPay exposes a scripting DSL
  (`BTC_USD = kraken(BTC_USD) ?? coinbase(BTC_USD)`) for per-store fallback
  chains. We deliberately choose **fixed robust aggregation (median +
  outlier) + declarative config** instead: simpler to audit, no operator
  scripting, and our use case (mid-market BTC/fiat for order pricing)
  needs resilience and correctness, not arbitrary per-pair logic. A DSL
  remains a possible future direction if real demand appears — noted as a
  non-goal for now.
- **Cross rates.** BTCPay derives crosses via explicit rules, not
  automatic triangulation. We do a **single, targeted** one-hop
  resolution for fiat-cross providers (`PerBase`, §6.3) — enough for
  El Toque — and treat general N-hop triangulation as a non-goal.
- **Bid/ask + spread.** BTCPay models bid/ask and lets stores add a
  spread. Mostro prices at **mid-market with no spread** (§6.6); the order
  premium/fee is the only markup. We call this out so the spread concept
  is not reintroduced by accident when porting a provider idea from
  BTCPay.

---

## 12. Tracking

Each phase ships as a separate PR linking this document. The PR
description states: which phase, which providers/config it touches, and
the test evidence (captured payloads + aggregation unit tests; a manual
"kill one API, watch the others carry the currency" check from Phase 2
on). When the full plan has landed, this spec stays in `docs/` as the
feature's reference.
