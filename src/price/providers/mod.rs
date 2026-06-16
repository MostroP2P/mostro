//! Per-provider adapters (spec §5.1, §5.4).
//!
//! Each file under this module implements [`super::PriceProvider`] for one
//! external price API. Adding a provider is one new adapter file + one
//! [`super::ProviderId`] variant + one registry arm in
//! [`super::PriceManager::from_settings`] + one config sub-table (see spec
//! §5.4). The aggregation core, store and scheduler are never touched.

pub mod blockchain;
pub mod coingecko;
pub mod currency_api;
pub mod eltoque;
pub mod yadio;
