//! Fixed-peg quoter for pegged codes (spec §11.8, §11.9).
//!
//! Mostro can trade bitcoin for an asset other than fiat by using the asset
//! as the order's currency (`f = USDT`, `f = RBTC`). Each code in
//! [`known_pegged`] is pegged 1:1 to something, so this adapter makes **no
//! network call**. Every tick it emits, per row:
//!
//! - a stablecoin ([`Peg::Fiat`]): [`Quote::PerBase`] `{ base: <fiat>,
//!   value: 1.0 }`, which the §6.3 resolution turns into the fiat's
//!   aggregated per-BTC price. A USDT market order is therefore priced
//!   exactly like a USD one, premium included; with no USD anchor this
//!   tick the coin keeps its last-known-good value and is refused past the
//!   TTL, the same dependency El Toque's CUP has (§11.3);
//! - bitcoin on another network ([`Peg::Btc`]): [`Quote::PerBtc`] of
//!   [`SATS_PER_BTC`], so `fa` units are `fa` sats. It needs no anchor at
//!   all.
//!
//! Why a fixed peg and not a market price, and the peg guards that are
//! separate follow-ups (S2, B2), are in the spec. Opt-in: the provider
//! ships disabled, and `only` / `except` pick the codes — scoping is
//! applied at the manager boundary like for every provider (§6.6), where
//! these codes are also admitted **only** from this provider.

use async_trait::async_trait;

use crate::price::config::ProviderConfig;
use crate::price::fiat::{known_pegged, Peg};
use crate::price::provider::{PriceProvider, ProviderError, ProviderId, ProviderQuotes, Quote};

/// Units of a BTC-pegged code per BTC: the unit is the sat (spec §11.9).
pub const SATS_PER_BTC: f64 = 100_000_000.0;

/// Fixed 1:1 peg quoter. Holds no state: the table is the data.
pub struct PeggedProvider;

impl PeggedProvider {
    /// Build the provider from its `[price.providers.pegged]`
    /// sub-table. Nothing is read from it: `only` / `except` are enforced
    /// by `ProviderConfig::validate` and the manager's scoping.
    pub fn new(_cfg: &ProviderConfig) -> Self {
        Self
    }
}

#[async_trait]
impl PriceProvider for PeggedProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Pegged
    }

    async fn fetch(&self, _http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
        Ok(known_pegged()
            .iter()
            .map(|(code, peg)| {
                let quote = match peg {
                    Peg::Fiat(fiat) => Quote::PerBase {
                        base: fiat.to_string(),
                        value: 1.0,
                    },
                    Peg::Btc => Quote::PerBtc(SATS_PER_BTC),
                };
                (code.to_string(), quote)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::price::fiat::peg_of;

    fn cfg() -> ProviderConfig {
        ProviderConfig {
            enabled: true,
            url: String::new(),
            fallback_urls: vec![],
            api_key: None,
            token: None,
            only: None,
            except: None,
            trusted_nodes: vec![],
        }
    }

    #[tokio::test]
    async fn fetch_quotes_every_code_at_one_to_one_with_its_peg() {
        let provider = PeggedProvider::new(&cfg());
        assert_eq!(provider.id(), ProviderId::Pegged);
        // Constructing a `reqwest::Client` performs no I/O, and `fetch`
        // never uses it.
        let quotes = provider.fetch(&reqwest::Client::new()).await.unwrap();
        assert_eq!(quotes.len(), known_pegged().len());
        assert_eq!(
            quotes.get("USDT"),
            Some(&Quote::PerBase {
                base: "USD".into(),
                value: 1.0
            })
        );
        // §11.9: 10⁸ units per BTC, a unit being a sat.
        for code in ["BTC", "LBTC", "RBTC"] {
            assert_eq!(
                quotes.get(code),
                Some(&Quote::PerBtc(100_000_000.0)),
                "{code}"
            );
        }
    }

    #[tokio::test]
    async fn only_btc_pegged_codes_are_quoted_directly() {
        // A stablecoin is only ever priced through its fiat's anchor; a
        // BTC-pegged code needs no anchor at all.
        let quotes = PeggedProvider::new(&cfg())
            .fetch(&reqwest::Client::new())
            .await
            .unwrap();
        for (code, quote) in &quotes {
            match peg_of(code) {
                Some(Peg::Fiat(fiat)) => assert_eq!(
                    quote,
                    &Quote::PerBase {
                        base: fiat.to_string(),
                        value: 1.0
                    },
                    "{code}"
                ),
                Some(Peg::Btc) => {
                    assert_eq!(quote, &Quote::PerBtc(SATS_PER_BTC), "{code}")
                }
                None => panic!("{code} is not in the table"),
            }
        }
    }
}
