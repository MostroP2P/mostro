//! Multi-source BTC/fiat price module (see `docs/PRICE_PROVIDERS.md`).
//!
//! Phase 0 — **foundation only**. This delivers the building blocks every
//! later phase reuses, with **no wiring**: the [`PriceProvider`] trait and
//! its data types ([`provider`]), the pure aggregation core
//! ([`aggregate`]), the in-memory aggregated-price store ([`store`]), and
//! the typed `[price]` configuration ([`config`]). Nothing here makes an
//! HTTP request, touches the scheduler, or is read by an order handler;
//! that begins in Phase 1.
//!
//! Because the module is not yet referenced from `main`/handlers, it would
//! otherwise trip the `dead_code` lint in a plain `cargo build`. The
//! allow below is scoped to this module and is **removed in Phase 1**,
//! when `PriceManager` wires the registry into the scheduler and
//! `get_bitcoin_price`.
#![allow(dead_code)]

pub mod aggregate;
pub mod config;
pub mod provider;
pub mod store;

pub use aggregate::{aggregate_tick, combine, resolve_per_base, AggregateResult};
pub use config::{PriceSettings, ProviderConfig};
pub use provider::{
    PriceProvider, ProviderError, ProviderHealth, ProviderId, ProviderQuotes, Quote,
};
pub use store::{AggregatedPrice, PriceError, PriceStore};
