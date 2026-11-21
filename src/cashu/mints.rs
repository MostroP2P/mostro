//! Connected [`CashuClient`]s, one per mint (issue #1046).
//!
//! A Cashu node escrows on whichever mint each order names, so the daemon
//! needs a client per mint rather than one bound at boot. Clients are built on
//! first use and cached by canonical URL ([`normalize_mint_url`]); the
//! configured mints are connected ahead of time at boot.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mostro_core::error::CantDoReason;
use tokio::sync::RwLock;

use super::mint_policy::{normalize_mint_url, public_mint_addr};
use super::{CashuClient, Error};

/// Most clients kept in the cache. On an open node makers choose the mints,
/// so the cache must not grow without bound; past the cap a client is still
/// built and used, just not kept.
const MAX_CACHED_MINTS: usize = 64;

/// How long a mint that failed to answer is left alone before the next
/// connection attempt. A silent mint costs the full request timeout per
/// attempt, and a failed attempt used to be forgotten at once, so a seller
/// could retry for ever. Chosen by eye, not measured.
const MINT_COOLDOWN: Duration = Duration::from_secs(30);

/// The node's Cashu mint clients.
pub struct CashuMints {
    /// `true` when `[cashu].mint_urls` is empty and any public mint is
    /// accepted. Uncached mints then get the host check again before mostrod
    /// connects, since the order was accepted some time ago, and the
    /// connection is pinned to the address that passed it.
    open: bool,
    clients: RwLock<HashMap<String, Arc<CashuClient>>>,
    /// Mints that failed to answer, and when. A mint in here is not contacted
    /// again until [`MINT_COOLDOWN`] has passed.
    cooling: RwLock<HashMap<String, Instant>>,
    /// Mints being connected right now. A second caller for the same mint is
    /// refused at once instead of waiting beside the first, so a silent mint
    /// costs one lock slot and not all of them.
    connecting: Mutex<HashSet<String>>,
}

/// One connection attempt in progress; frees the mint when dropped.
struct Attempt<'a> {
    mints: &'a CashuMints,
    key: String,
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if let Ok(mut connecting) = self.mints.connecting.lock() {
            connecting.remove(&self.key);
        }
    }
}

impl CashuMints {
    /// An empty registry. `open` is `true` when the node accepts any mint.
    pub fn new(open: bool) -> Self {
        Self {
            open,
            clients: RwLock::new(HashMap::new()),
            cooling: RwLock::new(HashMap::new()),
            connecting: Mutex::new(HashSet::new()),
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
        if self.is_cooling(&key).await {
            return Err(Error::MintConnection(format!(
                "{key}: did not answer a moment ago, not trying again yet"
            )));
        }
        let Some(_attempt) = self.begin_attempt(&key) else {
            return Err(Error::MintConnection(format!(
                "{key}: a connection attempt is already running, try again shortly"
            )));
        };
        match self.connect(&key).await {
            Ok(client) => Ok(self.keep(key, client).await),
            Err(e) => {
                if matches!(e, Error::MintConnection(_)) {
                    self.start_cooling(&key).await;
                }
                Err(e)
            }
        }
    }

    /// Claim the right to connect to `key`, or `None` when someone already is.
    fn begin_attempt(&self, key: &str) -> Option<Attempt<'_>> {
        let mut connecting = self.connecting.lock().ok()?;
        connecting.insert(key.to_string()).then(|| Attempt {
            mints: self,
            key: key.to_string(),
        })
    }

    /// Whether `key` failed to answer less than [`MINT_COOLDOWN`] ago.
    async fn is_cooling(&self, key: &str) -> bool {
        self.cooling
            .read()
            .await
            .get(key)
            .is_some_and(|since| since.elapsed() < MINT_COOLDOWN)
    }

    /// Leave `key` alone for [`MINT_COOLDOWN`]. Expired entries are dropped
    /// first, so the map cannot grow past what one cooldown window adds.
    async fn start_cooling(&self, key: &str) {
        let mut cooling = self.cooling.write().await;
        cooling.retain(|_, since| since.elapsed() < MINT_COOLDOWN);
        if cooling.len() < MAX_CACHED_MINTS {
            cooling.insert(key.to_string(), Instant::now());
        }
    }

    /// A mint that connected and then stopped answering during a lock: forget
    /// its client and leave it alone for [`MINT_COOLDOWN`].
    pub async fn mark_unreachable(&self, mint_url: &str) {
        let Ok(key) = normalize_mint_url(mint_url) else {
            return;
        };
        self.clients.write().await.remove(&key);
        self.start_cooling(&key).await;
    }

    async fn connect(&self, key: &str) -> Result<CashuClient, Error> {
        let key = key.to_string();
        let client = if self.open {
            // A maker chose this mint: connect to the exact address that
            // passed the host check, so DNS cannot be rebound in between.
            let addr = public_mint_addr(&key)
                .await
                .map_err(|reason| match reason {
                    // A resolver timeout is transient: the seller can retry.
                    CantDoReason::CashuMintUnavailable => {
                        Error::MintConnection(format!("{key}: resolving the mint host timed out"))
                    }
                    _ => Error::InvalidMintUrl(format!("{key}: host is not public")),
                })?;
            CashuClient::connect_pinned(&key, addr).await?
        } else {
            CashuClient::connect(&key).await?
        };
        Ok(client)
    }

    /// Cache a freshly connected client (unless the cache is full) and forget
    /// any cooldown. Two callers that connected at once share the first one.
    async fn keep(&self, key: String, client: CashuClient) -> Arc<CashuClient> {
        let client = Arc::new(client);
        let mut clients = self.clients.write().await;
        if let Some(existing) = clients.get(&key) {
            return existing.clone();
        }
        self.cooling.write().await.remove(&key);
        if clients.len() < MAX_CACHED_MINTS {
            clients.insert(key, client.clone());
        }
        client
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

    /// A mint that accepts connections and hangs up at once, counting them.
    async fn hanging_up_mint() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(socket);
            }
        });
        (url, accepted)
    }

    #[tokio::test]
    async fn a_mint_that_failed_is_left_alone_for_the_cooldown() {
        let (url, accepted) = hanging_up_mint().await;
        let mints = CashuMints::new(false);

        let first = mints.client_for(&url).await.err();
        assert!(matches!(first, Some(Error::MintConnection(_))));
        let contacted = accepted.load(std::sync::atomic::Ordering::SeqCst);
        assert!(contacted >= 1);

        // Within the cooldown nothing connects again.
        let second = mints.client_for(&url).await.err();
        assert!(matches!(second, Some(Error::MintConnection(_))));
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            contacted,
            "a mint in cooldown must not be contacted"
        );
    }

    #[tokio::test]
    async fn a_mint_is_tried_again_once_the_cooldown_has_passed() {
        let (url, accepted) = hanging_up_mint().await;
        let mints = CashuMints::new(false);
        let key = normalize_mint_url(&url).unwrap();
        assert!(mints.client_for(&url).await.is_err());
        let contacted = accepted.load(std::sync::atomic::Ordering::SeqCst);

        // Age the failure past the cooldown.
        mints.cooling.write().await.insert(
            key,
            Instant::now()
                .checked_sub(MINT_COOLDOWN + Duration::from_secs(1))
                .unwrap(),
        );
        assert!(mints.client_for(&url).await.is_err());
        assert!(
            accepted.load(std::sync::atomic::Ordering::SeqCst) > contacted,
            "after the cooldown the mint is contacted again"
        );
    }

    #[tokio::test]
    async fn the_cooldown_list_drops_expired_entries_and_never_passes_its_cap() {
        let mints = CashuMints::new(false);
        let old = Instant::now()
            .checked_sub(MINT_COOLDOWN + Duration::from_secs(1))
            .unwrap();
        {
            let mut cooling = mints.cooling.write().await;
            cooling.insert("https://expired.example.com".into(), old);
            cooling.insert("https://fresh.example.com".into(), Instant::now());
        }
        mints.start_cooling("https://new.example.com").await;
        {
            let cooling = mints.cooling.read().await;
            assert!(!cooling.contains_key("https://expired.example.com"));
            assert!(cooling.contains_key("https://fresh.example.com"));
            assert!(cooling.contains_key("https://new.example.com"));
        }

        // Full of live entries: one more is not added.
        {
            let mut cooling = mints.cooling.write().await;
            cooling.clear();
            for i in 0..MAX_CACHED_MINTS {
                cooling.insert(format!("https://m{i}.example.com"), Instant::now());
            }
        }
        mints
            .start_cooling("https://one-too-many.example.com")
            .await;
        let cooling = mints.cooling.read().await;
        assert_eq!(cooling.len(), MAX_CACHED_MINTS);
        assert!(!cooling.contains_key("https://one-too-many.example.com"));
    }

    #[tokio::test]
    async fn a_malformed_url_does_not_start_a_cooldown() {
        let mints = CashuMints::new(false);
        assert!(mints.client_for("ftp://mint.example.com").await.is_err());
        assert!(mints.cooling.read().await.is_empty());
    }

    #[tokio::test]
    async fn marking_a_mint_unreachable_drops_its_client_and_starts_a_cooldown() {
        let mints = CashuMints::new(false);
        mints
            .insert(CashuClient::offline("https://mint.example.com"))
            .await;
        assert!(mints.client_for("https://mint.example.com").await.is_ok());

        mints.mark_unreachable("https://MINT.example.com/").await;

        let err = mints.client_for("https://mint.example.com").await.err();
        assert!(
            matches!(err, Some(Error::MintConnection(ref m)) if m.contains("not trying again")),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_second_caller_does_not_wait_beside_a_connection_already_running() {
        // A mint that accepts and never answers: the first caller waits out the
        // request timeout, the second must be turned away at once.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let held = tokio::spawn(async move {
            let mut sockets = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                sockets.push(socket);
            }
        });
        let mints = Arc::new(CashuMints::new(false));
        let first = tokio::spawn({
            let (mints, url) = (mints.clone(), url.clone());
            async move { mints.client_for(&url).await.err() }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        let second = tokio::time::timeout(Duration::from_secs(1), mints.client_for(&url)).await;
        held.abort();
        first.abort();
        let err = second
            .expect("the second caller must not wait for the first")
            .err();
        assert!(
            matches!(err, Some(Error::MintConnection(ref m)) if m.contains("already running")),
            "got {err:?}"
        );
    }
}
