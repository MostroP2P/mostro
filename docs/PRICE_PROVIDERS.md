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

```text
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

```text
                 ┌─ yadio.fetch()    ─┐  (PerBtc)
poll all healthy ├─ coingecko.fetch()─┤  (PerBtc)   each with per-provider
providers, in    └─ eltoque.fetch()  ─┘  (PerBase)  timeout + circuit breaker
parallel
        │
        ▼
 1. collect PerBtc quotes  → per-currency candidate lists (the "anchors")
 2. resolve PerBase quotes → currency/BTC = value × aggregate(base/BTC)   (§6.3)
 3. aggregate per currency → median + outlier guard / mean / single       (§6.2)
 4. write store: { currency -> AggregatedPrice { value, as_of: observed_at, written_at, sources } }
    - `observed_at` is the tick's `now` for a directly-fetched rate, and the
      source event's own `created_at` for one relayed over Nostr (§6.4).
    - `written_at` is always the tick's `now`, even for a relayed rate — it
      answers "did our tick refresh it?" (the freshness warning), while
      `as_of` answers "how old is this price?" (the TTL).
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

Each currency's stored `AggregatedPrice` carries `as_of` = the time the
served value was **observed**, which is not in general the time of the tick
that stored it (see the first bullet).

- A tick that yields a fresh value overwrites the entry with `as_of` = when
  the value was **observed**. For a directly-fetched rate that is the tick's
  `now` (the HTTP request returns the rate as of now). For a rate relayed
  over Nostr it is the source event's own `created_at`, so the provider's
  acceptance window and this serving window do not stack: a relayed price is
  bounded at one `max_price_staleness_seconds` from observation, whatever age
  the event arrived with (issue #860). The same applies to a fiat-cross
  currency resolved against a Nostr-sourced anchor
  (`nostr_anchor_dependent`, §6.3), which is no fresher than that anchor.
- A **backdated** write never moves `as_of` backwards: one carrying an
  observation older than the one already stored is dropped, so a relayed rate
  predating a direct fetch cannot shorten a currency's remaining serving
  window. The guard defers only to a stamp the writing clock could still
  have produced (`prior.as_of <= now`): after a backwards clock step the
  held stamp sits in the future while every event the provider can still
  accept (`created_at <= now`) predates it, so an unconditional guard would
  drop every relayed write until the clock climbed back — the same freeze,
  one layer down. A **directly-fetched** write is not guarded at all, on
  purpose — it is this node's own authoritative observation and must land
  even when the wall clock has stepped backwards behind a stamp already
  held, since dropping it would freeze the price while still serving it as
  fresh.
- A tick with zero contributors for a currency leaves the prior entry
  untouched (old `as_of`).
- Because `as_of` can predate the tick, a fresh aggregate is **not** the same
  thing as a servable one: a relayed event admitted at the edge of the
  provider's acceptance window can already be past the TTL when it is
  written. The tick report therefore carries `servable_currencies` — every
  stored entry inside the TTL, which is what `get_price` would serve right
  now. It is deliberately not "fresh": an entry counted there may be old
  enough to trigger the stale warning while still being served, and it may
  come from an earlier tick. Both narrower counts mislead the operator, in
  opposite directions — the size of the tick's aggregate map names
  currencies the tick just stamped past the TTL, while restricting to the
  tick's own currencies omits the last-known-good values a partial outage
  leaves behind.
- `PriceManager::get_price(ccy)`:
  - entry missing → `Err(NoCurrency)`.
  - `now - as_of <= max_price_staleness_seconds` → `Ok(value)` (a `warn!`
    is logged once the value is older than one update interval, measured
    from `written_at` — when this node last wrote it — so a relayed rate
    whose backdated `as_of` is already older than one interval does not warn
    on a healthy node every tick).
  - else → `Err(PriceTooStale)` (§10.2). Market-priced create/take for
    that currency is refused with a clear message.

  The two warnings measure different clocks and say so in the log line:
  "not refreshed for Ns" (`written_at`) asks whether our tick is keeping
  up, "observed Ns ago" (`as_of`) how old the price itself is. For a
  relayed currency the two ages differ, so adjacent lines about one value
  are not the same measurement repeated.

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
- **Pegged-code allowlist (§11.8, §11.9).** A second, smaller table next
  to the fiat one, `KNOWN_PEGGED`, maps each pegged code to what it is
  pegged 1:1 to: a fiat for a stablecoin (`USDT` → `USD`), or bitcoin for
  bitcoin on another network (`RBTC`). Its codes pass the boundary **only
  from the `pegged` provider**. A source that also lists crypto
  (`currency-api`'s `usdt`, Yadio's `BTC`) and a trusted node's relayed
  rate (`nostr`) are still dropped for them. So nothing but the peg ever
  prices a pegged code, and a node that has not opted in never stores one.
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

# No network call: quotes each pegged code at 1:1 with its peg, a fiat
# (§11.8) or bitcoin (§11.9).
[price.providers.pegged]
enabled = false          # opt-in
# only = ["USDT", "RBTC"]  # optional; default: every code in the table
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
  provider is rejected. The `pegged` provider needs no `url` (like
  `nostr`), and its `only` / `except` entries must be codes from its
  table (§11.8, §11.9).

## 8. Phase overview

| Phase | PR scope | Depends on | Status |
|------:|----------|------------|--------|
| 0 | Foundation: `PriceProvider` trait, `Quote`, aggregation core (pure), store, `[price]` config types | — | done (PR #753) |
| 1 | Yadio provider + registry + scheduler wiring (single-source parity); `get_bitcoin_price` reads new store | 0 | done (PR #753) |
| 2 | Direct backup quoters (CoinGecko, currency-api, Blockchain.com) → real multi-source aggregation; per-provider health/circuit-breaker; currency normalisation + fiat allowlist + per-provider scoping | 1 | in review |
| 3 | El Toque provider (fiat-cross CUP/MLC) via PerBase anchor resolution | 2 | done (PR #778) |
| 4 | Unify `get_market_quote` onto the cache; staleness TTL enforcement (`PriceTooStale`) at create/take | 2 | done |
| 5 | Nostr aggregated publishing + token/paid-provider support polish + info-event exposure + retire `bitcoin_price.rs` + ops docs | 3, 4 | pending |

Phases 3 and 4 both depend on Phase 2 and can land in either order.

> **Out-of-band addition — Nostr *subscribe* (issue #697, §11.7).** The
> five phases above only ever covered *publishing* rates to Nostr (Phase 5).
> A separate, independently-landing addition — the `nostr` provider — makes
> `mostrod` able to *consume* another Mostro node's published rates instead
> of an HTTP API, for operators in regions where the price APIs are
> network-blocked. It slots into the existing registry (§5.4) like any
> other provider and does not otherwise depend on Phase 5.

> **Out-of-band addition — stablecoins (issue #1081, §11.8).** A provider
> that makes no network call and quotes USDT, USDC and DOC at 1:1 with
> USD through the §6.3 PerBase path, so a node that opts in can take
> market-priced orders in them. It lands as its own PR on top of Phase 4.
> It shipped as `stablecoins` (#1083); §11.9 renames it `pegged`.

> **Out-of-band addition — bitcoin on other networks (issue #1088,
> §11.9).** The same provider, renamed `pegged`, quotes on-chain BTC,
> L-BTC and RBTC at 10⁸ units per BTC, a unit being a sat, so a node that
> opts in can take market-priced orders that swap Lightning sats for them.

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
- `src/price/store.rs`: `AggregatedPrice { value, as_of, written_at, source_count }`,
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

> **Phase 4 shipped status.** `get_market_quote` (`util.rs`) is now a
> synchronous cache read — `sats = (fiat_amount / get_price(ccy)) × 1e8`
> with the premium applied — and the live Yadio path
> (`retries_yadio_request`, `yadio_base_url`, the `/convert` call, the
> `Yadio` response model) is deleted. With the live path gone, the legacy
> `[mostro].bitcoin_price_api_url` now feeds **only** legacy synthesis
> (§10.1).
>
> Staleness is enforced in `PriceManager::get_price`: a value past
> `max_price_staleness_seconds` returns
> `ServiceError::PriceTooStale` (still warning once on the transition).
> Order create (`app/order.rs`) and market-priced takes
> (`app/take_buy.rs`, `app/take_sell.rs`) map that onto a user-facing
> `CantDoReason::PriceTooStale`. The dedicated error variants landed in
> **mostro-core 0.13.1** (MostroP2P/mostro-core#153), replacing the
> interim `NoAPIResponse` reuse described in §10.2.
>
> **Known gap — bonded takes (follow-up).** With taker bonds enabled,
> the staleness check runs at take-time, but the resulting quote is
> snapshotted into the bond's `taker_*` columns and copied verbatim onto
> the order when the bond locks (`promote_taker_context_to_order`), with
> no re-pricing or staleness re-check. A taker who pays the bond after
> the TTL elapses therefore completes on a quote the direct path would
> refuse. Until the bond flow revalidates (or expires) the snapshot at
> lock time, the Phase 4 guarantee applies to the **direct (bond-less)
> path only**.
>
> **Legacy configs.** Enforcement does **not** require a `[price]`
> block: when the section is absent, §10.1 synthesises one (single
> Yadio provider) and staleness applies with the **default TTL of
> 1800 s**. Tuning `max_price_staleness_seconds` does require adding a
> `[price]` block, since the legacy `[mostro]` keys carry no staleness
> setting.

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
- **Behaviour during rollout.** Staleness was logged-only through Phases
  1–3 (never refusing an order that would have priced today); Phase 4
  turns enforcement on (§6.4), so a market-priced create/take is refused
  with `PriceTooStale` once the rate ages past the TTL.

### 10.2 mostro-core changes

- A new error for refused stale prices was needed:
  `ServiceError::PriceTooStale` (internal) and `CantDoReason::PriceTooStale`
  (user-facing) in `mostro-core`. **Shipped in mostro-core 0.13.1**
  (MostroP2P/mostro-core#153) — a serde-additive change. Phase 4 consumes
  these directly; the interim `NoAPIResponse` reuse mentioned here is no
  longer used.

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

> **Phase 3 shipped status.** The El Toque adapter
> (`src/price/providers/eltoque.rs`) is wired with **anchor = USD only**
> (Q2 above declined for this phase). The request and response are confirmed
> against the live API:
> - **Request:** `GET {url}/v1/trmi?date_from=…&date_to=…` with
>   `Authorization: Bearer <token>`. The endpoint requires a
>   `[date_from, date_to]` range (`YYYY-MM-DD HH:MM:SS`, URL-encoded) and
>   returns the most recent rate within it, so `fetch` queries a rolling
>   48h window ending "now".
> - **Response:** a CUP-denominated `tasas` object
>   (`{"tasas":{"USD":490.0,"MLC":200.0,"ECU":540.0,…}}`, where El Toque uses
>   `ECU` for the euro) plus the timestamp of the returned rate
>   (`date`/`hour`/`minutes`/`seconds`, which the parser ignores). The parse
>   path applies the §11.3 cross math and is fully unit-tested against a
>   captured fixture (`tests/fixtures/price/eltoque_trmi.json`).
>
> Q1/Q3 remain open. Keep `enabled = false` in production until a token is
> provisioned and the operator opts in.

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

### 11.7 Nostr (subscribe, trusted-node relay mode) — issue #697

Every source above is an HTTP API. This one is not: it sources quotes from
**other Mostro nodes**, over Nostr, for operators whose network blocks the
HTTP price APIs outright (the motivating case: `api.yadio.io` is DNS-blocked
for Venezuelan ISPs) but who can still reach a Nostr relay.

- **Not a new publish path.** Mostro nodes already publish their aggregated
  rates as a kind-30078 NIP-33 event (`d = "mostro-rates"`, content
  `{"BTC": {ccy: price}}`) — see `docs/NOSTR_EXCHANGE_RATES.md` and
  `nip33::new_exchange_rates_event`. The `nostr` provider is a **consumer**
  of that exact same event, from other operators' nodes instead of its own.
- **Config (`[price.providers.nostr]`, §7):** no `url` — `trusted_nodes` (a
  list of hex pubkeys) is the provider's only required field. Enabled
  without at least one valid `trusted_nodes` pubkey is a startup error
  (mirrors El Toque's missing-token fail-fast, §7).
- **Relays are not reconfigured.** The provider reuses the process-wide
  Nostr client already connected to the node's own `[nostr]` relays — it
  does not open a second set of connections or accept a separate relay list.
- **One-shot query per tick, not a persistent subscription.** `fetch()` is
  called once per `update_interval_seconds` tick like every other provider
  (spec §5.3); it issues a single `kind=30078, authors=trusted_nodes,
  #d=mostro-rates` relay query bounded by a short fixed timeout, rather than
  keeping an open `REQ` subscription running between ticks.
- **Freshest wins — no cross-node combine.** With several `trusted_nodes`
  configured, the provider does **not** run the §6.2 median+outlier combine
  across their individual events; it takes the single event with the
  highest `created_at` among the trusted, valid ones. Rationale (confirmed
  with the maintainer): the issue frames multiple `trusted_nodes` as
  *redundancy* (if one is down or stale, use another), not as independent
  samples to statistically combine, and skipping a second aggregation layer
  inside a single provider keeps the adapter's contract identical to every
  HTTP provider (one `ProviderQuotes` map out of `fetch()`, no new state).
  A compromised single trusted node can therefore still push a bad price
  with no in-provider outlier guard; operators mitigate by choosing
  `trusted_nodes` they actually trust, same as any other single-sourced
  provider.
- **Publish cadence bounds the serving window.** A relayed rate is stamped
  with the source event's `created_at`, not with when it arrived (§6.4), so
  its serving window on this node is `max_price_staleness_seconds` **minus
  the event's age on arrival** — not a full TTL from ingestion. A trusted
  node publishing anywhere near the TTL cadence produces currencies that go
  unservable rather than merely stale, where `main` before the #860 fix
  served them. Choose `trusted_nodes` whose publish cadence
  (`update_interval_seconds`, with `publish_to_nostr` enabled) is well under
  this node's `max_price_staleness_seconds`.
- **Client-side pubkey verification.** Even though the relay-side `authors`
  filter already restricts the query, the adapter re-checks each returned
  event's `pubkey` against `trusted_nodes` before trusting its content —
  the same verification `docs/NOSTR_EXCHANGE_RATES.md` requires of mobile
  clients, applied here on the operator side.
- **Currency codes are re-upper-cased** (§6.6) even though a Mostro
  publisher already emits uppercase codes — a third-party trusted node is
  outside this codebase's control.

### 11.8 Stablecoins (fixed 1:1 peg, opt-in) — issue #1081

Mostro can trade bitcoin for stablecoins by using the coin as the order's
currency: `f = USDT`, and the buyer pays in USDT instead of fiat. Each coin
is treated as a currency pegged 1:1 to a fiat. This section covers how the
node prices them. The wire side (the code list, the network in the payment
method) is the protocol book's
[stablecoin page](https://mostro.network/protocol/stablecoins.html); the
clients' side is MostroP2P/app#884 and MostroP2P/mostro-cli#213.

> **Renamed by §11.9.** S1 (#1083) shipped the provider as `stablecoins`
> and the table as `KNOWN_STABLECOINS`. §11.9 renames them `pegged` and
> `KNOWN_PEGGED`, before any release carries the old names, so that the
> same provider also quotes bitcoin on other networks. This section uses
> the new names. The stablecoin rules do not change.

**What works without this.** `fiat_code` is a free string, checked only
against `fiat_currencies_accepted`, so a fixed-price USDT order already
passes wherever that list allows the code. A market-priced one fails: no
provider quotes USDT, and the §6.6 fiat allowlist would drop it if one did,
so `get_price("USDT")` returns `NoCurrency`.

**The table.** In `src/price/fiat.rs`, next to `KNOWN_FIAT`, a stablecoin
is a `Peg::Fiat` row of `KNOWN_PEGGED` (the whole table and its `Peg` type
are in §11.9):

```rust
// The stablecoin rows of KNOWN_PEGGED: code → the fiat it is pegged 1:1 to.
("DOC", Peg::Fiat("USD")),
("USDC", Peg::Fiat("USD")),
("USDT", Peg::Fiat("USD")),
```

- Adding a coin is one row here and one in the protocol book's list.
  `EURC` → `EUR` is the next candidate.
- Tests hold that the table is sorted, that every `Peg::Fiat` peg is in
  `KNOWN_FIAT`, and that no code is in both tables.

**The provider.** `src/price/providers/pegged.rs`, `ProviderId::Pegged`
(config key `pegged`). `fetch` makes no network call: it returns
`Quote::PerBase { base: <peg>, value: 1.0 }` for every `Peg::Fiat` row of
the table, and the §6.3 resolution turns that into the peg's aggregate
per-BTC price. So:

- a USDT market order is priced exactly like a USD one, premium included;
- with no USD anchor this tick, USDT keeps its last-known-good value and is
  refused past the TTL, the same dependency El Toque's CUP has (§11.3);
- a USD anchor relayed over Nostr makes the coin `nostr_anchor_dependent`
  (§6.4): stamped with the relayed observation and not republished, as any
  fiat-cross;
- the coins land in the store and in `mostro-rates` like any currency, which
  is how clients learn which coins a node quotes.

**Why a fixed peg and not a market price.**

- **USDT and USDC** trade within about 0.1% of $1 on deep markets
  (CoinGecko, October 2026: USDT 0.99895, USDC 0.99958). The order
  `premium` is a whole percent, so an order cannot express the difference.
- **DOC** has no market deep enough to price it. CoinGecko quotes it (0.9939
  at the same time) from about $1,000 a day across two Rootstock DEX pools.
  That is a thin market's spread, cheap to move, and CoinGecko is the only
  source, so the median would not guard it.
- **1:1 is how DOC is defined.** Money on Chain never prices DOC:
  `MoCState.peg()` returns `1`, and the protocol mints and redeems DOC
  against RBTC at the BTC/USD price of its OMoC oracle (a weighted median of
  Coinbase, Bitstamp, Bitfinex, Kraken and Gemini). Every DOC is redeemable
  outside settlement (`freeDoc() == totalSupply()`), which is what holds
  the peg.

**The boundary rule (§6.6).** At the manager boundary a code passes when it
is a known fiat, or when it is a known pegged code **and** comes from the
`pegged` provider. `only` / `except` scoping then applies as for every
provider. Two consequences:

- `currency-api`'s `usdt`, or any API that also lists crypto, never
  contributes a market price for a coin. The price stays the peg.
- A trusted node's `mostro-rates`, relayed by the `nostr` provider (§11.7),
  carries USDT if that node opted in. It is dropped here. Otherwise a node
  that never opted in would start pricing USDT orders because a node it
  trusts for USD did. A node that did opt in loses nothing: its own
  `pegged` quote resolves against the relayed USD anchor.

**Opt-in.** The provider ships disabled. A node that accepts USD and VES
and wants USDT and USDC at market price, but not DOC:

```toml
[mostro]
fiat_currencies_accepted = ['USD', 'VES', 'USDT', 'USDC']

[price.providers.pegged]
enabled = true
only = ["USDT", "USDC"]   # optional; default: every code in the table
```

- The two settings answer different questions, as for every currency:
  `fiat_currencies_accepted` says which codes orders may use, the provider
  which of them have a market price. CUP works the same way: listed in the
  first, quoted by Yadio and El Toque.
- A coin can be accepted at fixed price only: list it and do not quote it.
- With the provider off, or no sub-table, the node behaves as today:
  fixed-price orders in these codes pass where the list allows them, and
  market-priced ones are refused for lack of a rate.
- **Startup validation:** no `url` is required (as for `nostr`), and an
  `only` / `except` entry that is not in the table is an error, since a
  typo would otherwise quote nothing without a word.
- **Startup warning:** when `fiat_currencies_accepted` is not empty and the
  provider quotes a coin the list does not name, the node logs a `warn!`
  once: it would publish a rate for that coin and refuse every order in it.
  The reverse, listed but not quoted, is the fixed-price-only case and is
  not warned.

**The network.** A stablecoin transfer is only defined with its network,
and a transfer sent on the wrong one can be lost. The network goes in the
order's payment method (`USDT (Tron)`), as the protocol book's stablecoin
page specifies; `pm` is a free string, so mostrod changes nothing for it. A
structured field was set aside: mostro-core and every client would change,
and a client that ignored the new tag would show "100 USDT" with no
network, the very failure the field is meant to prevent. (The kind-38383
event already has a `network` tag, for the Lightning network.)

**Amounts.** `fiat_amount` is an `i64`, so orders are in whole units, as
with fiat. (The BTC-pegged codes count sats instead, §11.9.)

**Depeg guard (follow-up S2, design open).** A fixed peg is wrong during a
depeg: USDC traded around $0.87 in March 2023. The guard is a separate
check, **not** part of the `pegged` provider: `fetch` stays
network-free, so S1 adds no network dependency, and a failing check never
takes the peg down with it. Proposed contract:

- **When:** once per price tick, before aggregation.
- **What it reads:**
  - USDT and USDC: CoinGecko
    (`simple/price?ids=tether,usd-coin&vs_currencies=usd`).
  - DOC: a market price says nothing, so the signals are on Rootstock. The
    contract is paused, or `globalCoverage()` is under its protected (1.5)
    or liquidation (1.04) threshold (7.6 in October 2026).
  - Later coins: their price against their own fiat, where a deep market
    exists (EURC has one).
- **On a depeg** (a coin more than a threshold, 3%?, off its fiat): the
  guard drops the coin's quote from the tick **and** evicts the code from
  the store, so market orders in it are refused at once. Dropping the quote
  alone is not enough: last-known-good would keep serving 1:1 for
  `max_price_staleness_seconds`. It logs at `warn`, and the coin comes back
  once a check finds it within the threshold again.
- **When the check itself fails** (CoinGecko or the Rootstock RPC is
  unreachable): the coin keeps its 1:1 quote, and the failure is logged. An
  outage of the checker is not a depeg, and failing closed would turn off
  market orders in these coins whenever the keyless CoinGecko tier is
  rate-limited.

Until it lands, an operator handles a depeg by taking the coin out of
`only`.

**Rollout.**

| Step | Repo | Scope | Depends on |
|------|------|-------|------------|
| S0 | protocol | Stablecoin page (codes, pegs, 1:1 rule, network in `pm`); `f` tag; `mostro-rates` | — |
| S1 | mostro | The table, the boundary rule, the provider, its validation and startup warning, `settings.tpl.toml` (done, #1083, under the names §11.9 replaces) | — |
| C1 | mostro-cli | Market-price check against the node's `mostro-rates` instead of Yadio (#213) | — |
| C2 | app | Design guide rule, catalogue, mark, default-currency rule, Market lock (#884, phase 1) | — |
| C3 | app | Payment methods by network, then the trade hint (#884, phases 2–3) | C2 |
| S2 | mostro | Depeg guard | S1 |
| — | all | EURC: a row in the table, in the book and in the app's catalogue | S1 |

S0, S1, C1 and C2 can land in any order. The book describes what a node
does *if* it quotes a coin, so it can merge first; and until a node quotes
one, the CLI refuses a market order in it and the app locks Market.

**Acceptance / tests (S1)**

- With the provider on and a USD anchor, `get_price("USDT")` equals
  `get_price("USD")`.
- No USD anchor: USDT serves last-known-good, then `PriceTooStale` past the
  TTL.
- `currency-api`'s `usdt` and a Nostr-relayed `USDT` are dropped at the
  boundary; the provider's own quote is kept.
- `only = ["USDT"]` quotes USDT and not USDC; `only = ["USTD"]` fails at
  startup.
- Provider off: no `USDT` in the store or in `mostro-rates`, and a
  fixed-price USDT order still passes with an empty
  `fiat_currencies_accepted`.
- The startup warning fires for a quoted coin missing from a non-empty
  list, and not for a listed coin that is not quoted.
- The table tests above.

### 11.9 Bitcoin on other networks (1:1 in sats, opt-in) — issue #1088

Mostro can swap Lightning sats for bitcoin on another network the way it
trades stablecoins (§11.8): the asset is the order's currency, and the
buyer pays in it. There are three codes, each pegged 1:1 to bitcoin itself:

| Code | Asset | Network | Decimals |
|------|-------|---------|---------:|
| `BTC` | bitcoin | Bitcoin, on-chain | 8 |
| `LBTC` | L-BTC | Liquid | 8 |
| `RBTC` | RBTC | Rootstock | 18 |

This section covers the unit, the price, the rename that lets §11.8's
provider quote these codes, and a rounding fix they need. The wire side is
the protocol book's (step B0 below).

**What works without this.** As for USDT, a fixed-price `RBTC` order
already passes wherever `fiat_currencies_accepted` allows the code, and a
market-priced one fails with `NoCurrency`. The §6.6 allowlist also drops
Yadio's `BTC` self-quote (bitcoin priced in bitcoin), and must keep doing
so: that rate would price a sat-denominated `fa` 10⁸ off. What nothing
defines today is what `fa` counts for these codes.

**The unit is the sat.** `fiat_amount` is an `i64`, so in whole units the
smallest order would be 1 BTC. For a BTC-pegged code, `fa` (and a range
order's `min` and `max`) counts sats of the asset: `f = RBTC`,
`fa = 100000` is 0.001 RBTC.

- Nothing changes type in mostro-core, the database or the wire.
- BTC and L-BTC have 8 decimals, so the sat is their smallest unit. RBTC
  has 18; a client rounds an RBTC amount to 10⁻⁸ before it builds the
  order. mostrod only sees `fa`.
- `min_payment_amount` and `max_order_amount` bound the order's sats as
  they do today. Here the sats are `fa` less the premium.

**The table and the rename.** §11.8's provider already makes no network
call, and the boundary admits its codes only from it, which is what these
codes need. So the provider takes them too, and its names change to say
what its codes share. In `src/price/fiat.rs`:

```rust
/// What a pegged code is pegged 1:1 to.
pub enum Peg {
    /// A fiat in `KNOWN_FIAT`; `fa` in whole units (§11.8).
    Fiat(&'static str),
    /// Bitcoin; `fa` in sats (§11.9).
    Btc,
}

/// Pegged code → its peg. Sorted by code.
const KNOWN_PEGGED: &[(&str, Peg)] = &[
    ("BTC", Peg::Btc),
    ("DOC", Peg::Fiat("USD")),
    ("LBTC", Peg::Btc),
    ("RBTC", Peg::Btc),
    ("USDC", Peg::Fiat("USD")),
    ("USDT", Peg::Fiat("USD")),
];
```

| Shipped in #1083 | Renamed to |
|------------------|------------|
| `[price.providers.stablecoins]` | `[price.providers.pegged]` |
| `ProviderId::Stablecoins` | `ProviderId::Pegged` |
| `src/price/providers/stablecoins.rs`, `StablecoinsProvider` | `src/price/providers/pegged.rs`, `PeggedProvider` |
| `KNOWN_STABLECOINS` | `KNOWN_PEGGED` |
| `stablecoin_peg`, `is_known_stablecoin`, `known_stablecoins` | `peg_of`, `is_known_pegged`, `known_pegged` |
| `quoted_but_unaccepted_stablecoins` | `quoted_but_unaccepted_pegged` |

- **No alias.** #1083 is in no release (0.20.1 predates it), so no
  operator's settings carry the old key. B1 has to land before the next
  release; if it does not, the old key needs an alias for a release.
- A `stablecoins` sub-table left from a build of `main` becomes an unknown
  id: `PriceManager::from_settings` warns and ignores it, and its coins
  lose their market price until the key is renamed.
- **Why `pegged` and not `cryptocurrency`.** ETH and XMR are
  cryptocurrencies, and a fixed peg cannot price them. A name that promises
  them invites `only = ["ETH"]`, which validation rejects. What these codes
  share is the peg.
- Table tests: the table is sorted, every `Peg::Fiat` peg is in
  `KNOWN_FIAT`, and no code is in both `KNOWN_FIAT` and `KNOWN_PEGGED`.

**The quote.** `fetch` returns, for each row:

- `Peg::Fiat(peg)`: `Quote::PerBase { base: peg, value: 1.0 }`, as in
  §11.8;
- `Peg::Btc`: `Quote::PerBtc(100_000_000.0)`, 10⁸ units per BTC, a unit
  being a sat.

So, for a BTC-pegged code:

- `get_market_quote` gives `sats = fa × (1 − premium/100)`. A sell order
  with `f = RBTC`, `fa = 100000`, market price and a 1% premium: the seller
  locks 99,000 Lightning sats and receives 100,000 sats of RBTC. The
  premium is the price of the swap.
- The quote needs no other rate. It is not a fiat-cross, so no anchor,
  relayed or not, makes it `nostr_anchor_dependent` (§6.4).
- The provider quotes it every tick, so it stays fresh while the scheduler
  runs, and goes stale like any code if the scheduler stops.
- It lands in the store and in `mostro-rates` at 100000000, which is how
  clients learn which codes a node quotes.

**Why 1:1.**

- **BTC** on-chain is bitcoin. There is no peg to price.
- **L-BTC and RBTC** exist against BTC locked in their pegs (the Liquid
  Federation and Rootstock's PowPeg) and are redeemed 1:1 through them.
  Crossing a peg costs fees and time. The order `premium` is where a maker
  prices that, and a market feed for it would add a network dependency to
  price a difference the premium already carries.

**The boundary rule (§6.6).** Unchanged:
`is_known_fiat(code) || (id == ProviderId::Pegged && is_known_pegged(code))`.
For these codes it means:

- Yadio's `BTC` self-quote, or any API that lists bitcoin among its
  currencies, never prices them.
- A trusted node's relayed `RBTC` is dropped, as a relayed `USDT` is. A
  node that opted in loses nothing: its own quote needs no anchor.

**Opt-in.** As in §11.8, the provider ships disabled, and two settings
answer two questions: `fiat_currencies_accepted` says which codes orders
may use, and the provider says which of them have a market price.

- Without `only`, the provider quotes every code in the table, of both
  kinds. An operator who turns it on for USDT alone also quotes RBTC. With
  a non-empty `fiat_currencies_accepted` those orders are still refused and
  the startup warning names the codes; with an empty one they are
  accepted. Whether the default should stay "every code" is open in #1088.
- The startup validation and warning cover the new codes unchanged.

**Configuration examples.**

1. Swaps only. A node that keeps trading fiat and adds Lightning ↔
   Liquid and Lightning ↔ Rootstock swaps at market price:

   ```toml
   [mostro]
   fiat_currencies_accepted = ['USD', 'VES', 'LBTC', 'RBTC']

   [price.providers.pegged]
   enabled = true
   only = ["LBTC", "RBTC"]
   ```

2. Stablecoins and swaps:

   ```toml
   [mostro]
   fiat_currencies_accepted = ['USD', 'VES', 'USDT', 'LBTC', 'RBTC']

   [price.providers.pegged]
   enabled = true
   only = ["USDT", "LBTC", "RBTC"]
   ```

3. On-chain BTC at fixed price only. The code is accepted but not quoted:
   a maker sets both amounts, and a market-priced `BTC` order is refused.

   ```toml
   [mostro]
   fiat_currencies_accepted = ['USD', 'BTC']

   [price.providers.pegged]
   enabled = false
   ```

4. Everything. An empty list accepts every code, and the provider without
   `only` quotes every code in the table:

   ```toml
   [mostro]
   fiat_currencies_accepted = []

   [price.providers.pegged]
   enabled = true
   ```

5. A mistake the startup warning catches. RBTC is quoted but not accepted,
   so the node would publish its rate and refuse every order in it:

   ```toml
   [mostro]
   fiat_currencies_accepted = ['USD', 'LBTC']

   [price.providers.pegged]
   enabled = true
   only = ["LBTC", "RBTC"]   # warns: RBTC is quoted but not accepted
   ```

**Orders under example 1.** `amt` is the order's sats (`amount`); `0`
means market price.

| Order | `f` | `fa` | `amt` | `premium` | Sats of the order |
|-------|-----|------|------:|----------:|-------------------|
| Market, with premium | `RBTC` | `100000` | `0` | `1` | 99,000 |
| Market, no premium | `LBTC` | `250000` | `0` | `0` | 250,000 |
| Fixed price | `LBTC` | `100000` | `99500` | — | 99,500 |
| Range, market | `RBTC` | `min 50000`, `max 200000` | `0` | `0` | the `fa` the taker picks, in sats |
| Market, refused | `BTC` | `100000` | `0` | `0` | — (`BTC` is not accepted) |

**`settings.tpl.toml` (B1).** B1 replaces the template's `stablecoins`
block with this one, so an operator learns from the template alone what
each kind of code is, its unit, and how to pick codes:

```toml
# # Pegged currencies as order currencies (docs/PRICE_PROVIDERS.md §11.8,
# # §11.9). No network call: each code is quoted at 1:1 with what it is
# # pegged to. Opt-in: off, the node behaves as today (fixed-price orders in
# # these codes pass where `fiat_currencies_accepted` allows them;
# # market-priced ones are refused for lack of a rate).
# #
# # Stablecoins, pegged to a fiat. `fa` is in whole coins, priced from the
# # node's rate for that fiat, so a USDT market order is priced like a USD
# # one. The network goes in the order's payment method ("USDT (Tron)",
# # "DOC (Rootstock)"); the node does not check it.
# #   USDT, USDC, DOC -> USD
# #
# # Bitcoin on other networks, pegged to bitcoin. `fa` is in SATS of the
# # asset (fa = 100000 is 0.001 RBTC). At market price the order is `fa`
# # sats less the premium. The code names the network.
# #   BTC (on-chain), LBTC (Liquid), RBTC (Rootstock)
# #
# # A non-empty `fiat_currencies_accepted` must also list every code you
# # quote, or every order in it is refused; the node warns at startup.
# # To accept a code at fixed price only, list it there and leave it out
# # of `only`.
# #
# # `only` picks the codes; without it, every code above is quoted:
# #   only = ["USDT", "USDC"]          # dollar stablecoins
# #   only = ["LBTC", "RBTC"]          # Lightning <-> Liquid / Rootstock swaps
# #   only = ["USDT", "BTC", "RBTC"]   # both kinds
# [price.providers.pegged]
# enabled = false
# # only = ["USDT", "USDC"]
```

The `[mostro]` comment on `fiat_currencies_accepted` names the pegged
codes too:

```toml
# Currencies accepted for orders: ISO 4217 fiat codes, plus the pegged codes
# of [price.providers.pegged] (stablecoins such as USDT; BTC, LBTC and RBTC,
# whose amounts are in sats). Leave empty [] to accept every code.
fiat_currencies_accepted = ['USD', 'EUR', 'ARS', 'CUP']
```

**Rounding.** The fiat-to-sats conversion truncates (`as i64`) in four
places:

- `get_market_quote` (`src/util.rs`);
- both branches of `maker_bond_notional_sats` (`src/util.rs`);
- `calculate_and_check_quote` (`src/app/order.rs`).

At 10⁸ units per BTC the float error shows: `(fa / 1e8) × 1e8` falls just
under `fa` for 159,879 of the amounts from 1 to 2,000,000, and truncation
then loses a sat (`fa = 29` gives 28). The four sites round to the nearest
sat instead, once and after the premium, through one shared helper.
Rounding gets every amount from 1 to 2,000,000 right, and every amount in
a random sample of 200,000 up to 21M BTC.

For fiat orders, the change moves about half of the quotes up one sat
(4,836 of the amounts from 1 to 10,000 at 108,000 per BTC).
`maker_bond_notional_range_sizes_against_max_at_price` pins 166,666, which
becomes 166,667.

**The network is the code.** `BTC`, `LBTC` and `RBTC` each name their
network, so unlike `USDT (Tron)` the payment method needs no network.

**Settlement and disputes.** Nothing here changes mostrod. It is for the
book, the clients and the solver guide.

- **Confirmations.** The seller releases only after the transfer confirms,
  never on zero confirmations. The window is the hold invoice's life
  (`hold_invoice_cltv_delta`, 144 blocks or about a day by default).
  Liquid and Rootstock confirm in minutes. An on-chain BTC transfer with a
  low fee can stay unconfirmed past the window, so a client tells the
  sender to pay a fee that confirms within hours.
- **Fees.** As for stablecoins, the sender pays the network fee and the
  receiver gets the full amount. On-chain, the fee can be a large share of
  a small order.
- **Disputes.** BTC and RBTC transfers are public, and a solver checks them
  in an explorer. Liquid transactions are confidential (amounts and assets
  are blinded), so the solver needs the unblinded link from the sender.

**Compatibility.** A NIP-69 client that checks for ISO codes ignores these
orders. A client that shows any code but does not know the unit would show
"100000 RBTC" for 0.001 RBTC. The provider ships off, and the book and the
clients land first. Any client can publish a fixed-price order in these
codes today, so the unit rule matters before any node quotes them.

**Peg guard (follow-up B2, design open).** On-chain BTC needs none. L-BTC
depends on the Liquid Federation and RBTC on the PowPeg, and a failure of
either breaks the 1:1. Like §11.8's depeg guard (S2), the guard would be
a separate check that
drops and evicts the code on a failure signal, and keeps the quote when
the check itself fails. Which signals to read is open. Until it lands, an
operator removes the code from `only`.

**Rollout.**

| Step | Repo | Scope | Depends on |
|------|------|-------|------------|
| B0 | protocol | The BTC-pegged codes (codes, 1:1 rule, `fa` in sats), on the stablecoin page or a sibling; `f` and `fa` tags; `mostro-rates` | — |
| R1 | mostro | Rounding: a `test:` commit that fails on `main`, then the shared helper | — |
| B1 | mostro | The rename, `Peg`, the `Peg::Btc` rows and their quote, the `settings.tpl.toml` text above; before the next release | R1 |
| C4 | mostro-cli | The three codes, amounts in sats; the market check is C1's | C1 |
| C5 | app | The codes in the catalogue with marks, sats entry and display, the Market lock of C2 | C2 |
| B2 | mostro | Peg guard for L-BTC and RBTC | B1 |

B0 and R1 can land in any order. Until a node quotes a code, the CLI
refuses a market order in it and the app locks Market, as for stablecoins.

**Acceptance / tests (R1)**

- At a price of 10⁸ per BTC with no premium, `get_market_quote` returns
  `fa` for every `fa` from 1 to 2,000,000. On `main` it returns 28 for 29.
- With a 1% premium, `fa = 100000` gives 99,000.
- `maker_bond_notional_sats` and `calculate_and_check_quote` round the
  same way.

**Acceptance / tests (B1)**

- Provider on, no other provider and no USD anchor: `get_price("RBTC")` is
  10⁸, and `get_market_quote(&100_000, "RBTC", 1)` is 99,000.
- Yadio's `BTC` and a Nostr-relayed `RBTC` are dropped at the boundary;
  the provider's own quote is kept.
- `only = ["RBTC"]` quotes RBTC and no stablecoin; `only = ["RBT"]` fails
  at startup.
- Provider off: no `RBTC` in the store or in `mostro-rates`, and a
  fixed-price RBTC order still passes with an empty
  `fiat_currencies_accepted`.
- The startup warning names a quoted BTC-pegged code missing from a
  non-empty list.
- The template's `pegged` block, uncommented, loads, and each of its
  `only` examples passes validation.
- §11.8's S1 tests pass under the new names, and the table tests above
  hold.

---

## 12. Tracking

Each phase ships as a separate PR linking this document. The PR
description states: which phase, which providers/config it touches, and
the test evidence (captured payloads + aggregation unit tests; a manual
"kill one API, watch the others carry the currency" check from Phase 2
on). When the full plan has landed, this spec stays in `docs/` as the
feature's reference.
