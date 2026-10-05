//! Connected [`CashuClient`]s, one per mint (issue #1046).
//!
//! A Cashu node escrows on whichever mint each order names, so the daemon
//! needs a client per mint rather than one bound at boot. Clients are built on
//! first use and cached by canonical URL ([`normalize_mint_url`]); the
//! configured mints are connected ahead of time at boot.

use std::collections::HashMap;
use std::sync::Arc;

use mostro_core::error::CantDoReason;
use tokio::sync::RwLock;

use super::mint_policy::{ensure_public_mint_host, normalize_mint_url};
use super::{CashuClient, Error};

/// Most clients kept in the cache. On an open node makers choose the mints,
/// so the cache must not grow without bound; past the cap a client is still
/// built and used, just not kept.
const MAX_CACHED_MINTS: usize = 64;

/// The node's Cashu mint clients.
pub struct CashuMints {
    /// `true` when `[cashu].mint_urls` is empty and any public mint is
    /// accepted. Uncached mints then get the host check again before mostrod
    /// connects, since the order was accepted some time ago.
    open: bool,
    clients: RwLock<HashMap<String, Arc<CashuClient>>>,
}

impl CashuMints {
    /// An empty registry. `open` is `true` when the node accepts any mint.
    pub fn new(open: bool) -> Self {
        Self {
            open,
            clients: RwLock::new(HashMap::new()),
        }
    }

    /// Connect each configured mint ahead of time. A mint that fails is
    /// logged and left out; it is retried on first use, and an order on it
    /// fails to lock with `CashuMintUnavailable` until it is back.
    pub async fn connect_configured(mint_urls: &[String]) -> Self {
        let mints = Self::new(mint_urls.is_empty());
        for mint_url in mint_urls {
            match mints.client_for(mint_url).await {
                Ok(_) => tracing::info!("Connected Cashu mint {mint_url}"),
                Err(e) => tracing::warn!(
                    "Cashu mint {mint_url} is unreachable; orders on it cannot lock until it is back ({e})"
                ),
            }
        }
        mints
    }

    /// The client for `mint_url`, connecting it on first use.
    pub async fn client_for(&self, mint_url: &str) -> Result<Arc<CashuClient>, Error> {
        let key = normalize_mint_url(mint_url).map_err(Error::InvalidMintUrl)?;
        if let Some(client) = self.clients.read().await.get(&key) {
            return Ok(client.clone());
        }
        if self.open {
            ensure_public_mint_host(&key)
                .await
                .map_err(|reason| match reason {
                    // A resolver timeout is transient: the seller can retry.
                    CantDoReason::CashuMintUnavailable => {
                        Error::MintConnection(format!("{key}: resolving the mint host timed out"))
                    }
                    _ => Error::InvalidMintUrl(format!("{key}: host is not public")),
                })?;
        }
        let client = Arc::new(CashuClient::connect(&key).await?);
        let mut clients = self.clients.write().await;
        if let Some(existing) = clients.get(&key) {
            return Ok(existing.clone());
        }
        if clients.len() < MAX_CACHED_MINTS {
            clients.insert(key, client.clone());
        }
        Ok(client)
    }

    /// Cache `client` under its own mint URL, for tests that must not reach
    /// a mint.
    #[cfg(test)]
    pub(crate) async fn insert(&self, client: CashuClient) {
        let key = normalize_mint_url(&client.mint_url().to_string()).expect("valid mint url");
        self.clients.write().await.insert(key, Arc::new(client));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lnurl::{allow_private_lnurl_hosts_for_test, AllowPrivateLnurlHostsGuard};

    #[tokio::test]
    async fn client_for_returns_the_cached_client_for_any_spelling() {
        let mints = CashuMints::new(false);
        mints
            .insert(CashuClient::offline("https://mint.example.com"))
            .await;
        let client = mints
            .client_for("HTTPS://Mint.example.com/")
            .await
            .expect("cached client");
        assert_eq!(
            client.mint_url().to_string().trim_end_matches('/'),
            "https://mint.example.com"
        );
    }

    #[tokio::test]
    async fn client_for_rejects_a_malformed_url() {
        let mints = CashuMints::new(false);
        let err = mints.client_for("ftp://mint.example.com").await.err();
        assert!(matches!(err, Some(Error::InvalidMintUrl(_))));
    }

    #[tokio::test]
    async fn client_for_reports_an_unreachable_mint() {
        let mints = CashuMints::new(false);
        let err = mints.client_for("http://127.0.0.1:1").await.err();
        assert!(matches!(err, Some(Error::MintConnection(_))));
    }

    #[tokio::test]
    async fn open_node_refuses_to_connect_a_non_public_mint() {
        let _lock = AllowPrivateLnurlHostsGuard::lock_policy().await;
        allow_private_lnurl_hosts_for_test(false);
        let mints = CashuMints::new(true);
        let err = mints.client_for("http://127.0.0.1:1").await.err();
        assert!(matches!(err, Some(Error::InvalidMintUrl(_))));
    }

    #[tokio::test]
    async fn connect_configured_keeps_booting_past_an_unreachable_mint() {
        let mints = CashuMints::connect_configured(&["http://127.0.0.1:1".to_string()]).await;
        assert!(!mints.open);
        assert!(mints.clients.read().await.is_empty());
    }
}
