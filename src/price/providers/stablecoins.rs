//! Stablecoin fixed-peg quoter (spec §11.8).
//!
//! Mostro can trade bitcoin for a stablecoin by using the coin as the
//! order's currency (`f = USDT`). Each coin is treated as a currency pegged
//! 1:1 to a fiat, so this adapter makes **no network call**: every tick it
//! emits [`Quote::PerBase`] `{ base: <peg>, value: 1.0 }` for every row of
//! [`known_stablecoins`], and the §6.3 resolution turns that into the peg's
//! aggregated per-BTC price. A USDT market order is therefore priced
//! exactly like a USD one, premium included; with no USD anchor this tick
//! the coin keeps its last-known-good value and is refused past the TTL,
//! the same dependency El Toque's CUP has (§11.3).
//!
//! Why a fixed peg and not a market price, and the depeg guard that is a
//! separate follow-up (S2), are in the spec. Opt-in: the provider ships
//! disabled, and `only` / `except` pick the coins — scoping is applied at
//! the manager boundary like for every provider (§6.6), where these codes
//! are also admitted **only** from this provider.

use async_trait::async_trait;

use crate::price::config::ProviderConfig;
use crate::price::fiat::known_stablecoins;
use crate::price::provider::{PriceProvider, ProviderError, ProviderId, ProviderQuotes, Quote};

/// Fixed 1:1 peg quoter. Holds no state: the table is the data.
pub struct StablecoinsProvider;

impl StablecoinsProvider {
    /// Build the provider from its `[price.providers.stablecoins]`
    /// sub-table. Nothing is read from it: `only` / `except` are enforced
    /// by `ProviderConfig::validate` and the manager's scoping.
    pub fn new(_cfg: &ProviderConfig) -> Self {
        Self
    }
}

#[async_trait]
impl PriceProvider for StablecoinsProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Stablecoins
    }

    async fn fetch(&self, _http: &reqwest::Client) -> Result<ProviderQuotes, ProviderError> {
        Ok(known_stablecoins()
            .iter()
            .map(|(code, peg)| {
                (
                    code.to_string(),
                    Quote::PerBase {
                        base: peg.to_string(),
                        value: 1.0,
                    },
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn fetch_quotes_every_coin_at_one_to_one_with_its_peg() {
        let provider = StablecoinsProvider::new(&cfg());
        assert_eq!(provider.id(), ProviderId::Stablecoins);
        // Constructing a `reqwest::Client` performs no I/O, and `fetch`
        // never uses it.
        let quotes = provider.fetch(&reqwest::Client::new()).await.unwrap();
        assert_eq!(quotes.len(), known_stablecoins().len());
        for (code, peg) in known_stablecoins() {
            assert_eq!(
                quotes.get(*code),
                Some(&Quote::PerBase {
                    base: peg.to_string(),
                    value: 1.0
                }),
                "{code} must be quoted at 1:1 with {peg}"
            );
        }
        assert_eq!(
            quotes.get("USDT"),
            Some(&Quote::PerBase {
                base: "USD".into(),
                value: 1.0
            })
        );
    }

    #[tokio::test]
    async fn fetch_never_emits_a_direct_quote() {
        let quotes = StablecoinsProvider::new(&cfg())
            .fetch(&reqwest::Client::new())
            .await
            .unwrap();
        assert!(
            quotes.values().all(|q| matches!(q, Quote::PerBase { .. })),
            "a stablecoin is only ever priced through its peg's anchor"
        );
    }
}
