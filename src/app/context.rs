//! Application context for dependency injection.
//!
//! `AppContext` holds all shared dependencies needed by handler functions,
//! replacing direct access to global state (`get_db_pool()`, `get_nostr_client()`,
//! `Settings::get_mostro()`, etc.).
//!
//! This enables unit testing with mock implementations — see `TestContextBuilder`.

use crate::config::settings::Settings;
use mostro_core::prelude::Message;
use nostr_sdk::{Client, Keys, PublicKey};
use sqlx::{Pool, Sqlite};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared application context passed to all handler functions.
///
/// Instead of reaching for globals, handlers receive `&AppContext` which
/// holds references to the database pool, Nostr client, and configuration.
///
/// # Example
///
/// ```rust,ignore
/// // DI entry point — extracts pool from ctx internally
/// cancel_action_with_ctx(&ctx, msg, event, my_keys, &mut ln_client).await?;
///
/// // Or access dependencies directly:
/// let pool = ctx.pool();
/// let settings = ctx.settings();
/// ```
/// Shared queue type used for outbound order messages.
pub type OrderMsgQueue = Arc<RwLock<Vec<(Message, PublicKey)>>>;

#[derive(Clone)]
pub struct AppContext {
    pool: Arc<Pool<Sqlite>>,
    nostr_client: Client,
    settings: Arc<Settings>,
    order_msg_queue: OrderMsgQueue,
    keys: Keys,
}

impl AppContext {
    /// Create a new application context.
    pub fn new(
        pool: Arc<Pool<Sqlite>>,
        nostr_client: Client,
        settings: Arc<Settings>,
        order_msg_queue: OrderMsgQueue,
        keys: Keys,
    ) -> Self {
        Self {
            pool,
            nostr_client,
            settings,
            order_msg_queue,
            keys,
        }
    }

    /// Database connection pool.
    pub fn pool(&self) -> &Pool<Sqlite> {
        &self.pool
    }

    /// Cloned database connection pool (`Arc`) for `'static` tasks/spawns.
    pub fn pool_arc(&self) -> Arc<Pool<Sqlite>> {
        self.pool.clone()
    }

    /// Nostr client for publishing events.
    pub fn nostr_client(&self) -> &Client {
        &self.nostr_client
    }

    /// Full application settings.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Shared queue for outbound order messages.
    pub fn order_msg_queue(&self) -> &OrderMsgQueue {
        &self.order_msg_queue
    }

    /// Mostro's Nostr signing keys.
    ///
    /// Parsed once at startup from `settings.nostr.nsec_privkey_file`.
    /// Use this instead of `get_keys()` to avoid re-parsing on every call.
    pub fn keys(&self) -> &Keys {
        &self.keys
    }
}

#[cfg(test)]
pub mod test_utils {
    use super::*;
    use crate::config::types::{
        DatabaseSettings, ExpirationSettings, LightningSettings, MostroSettings, NostrSettings,
        RpcSettings,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Test helper wrapper for inspecting the shared order-message queue.
    #[derive(Debug, Clone)]
    pub struct MockOrderMsgQueue {
        queue: OrderMsgQueue,
    }

    impl MockOrderMsgQueue {
        pub fn new() -> Self {
            Self {
                queue: Arc::new(RwLock::new(Vec::new())),
            }
        }

        pub fn queue(&self) -> OrderMsgQueue {
            self.queue.clone()
        }

        pub async fn len(&self) -> usize {
            self.queue.read().await.len()
        }

        pub async fn is_empty(&self) -> bool {
            self.queue.read().await.is_empty()
        }

        pub async fn clear(&self) {
            self.queue.write().await.clear();
        }
    }

    impl Default for MockOrderMsgQueue {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Builder for creating test contexts with mock/test dependencies.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let ctx = TestContextBuilder::new()
    ///     .with_pool(test_pool)
    ///     .with_settings(test_settings)
    ///     .build();
    /// ```
    pub struct TestContextBuilder {
        pool: Option<Arc<Pool<Sqlite>>>,
        nostr_client: Option<Client>,
        settings: Option<Arc<Settings>>,
        order_msg_queue: Option<OrderMsgQueue>,
        mock_order_msg_queue: Option<MockOrderMsgQueue>,
        keys: Option<Keys>,
    }

    impl TestContextBuilder {
        pub fn new() -> Self {
            Self {
                pool: None,
                nostr_client: None,
                settings: None,
                order_msg_queue: None,
                mock_order_msg_queue: None,
                keys: None,
            }
        }

        /// Use a specific database pool (e.g., in-memory SQLite for tests).
        pub fn with_pool(mut self, pool: Arc<Pool<Sqlite>>) -> Self {
            self.pool = Some(pool);
            self
        }

        /// Use a specific Nostr client for tests.
        pub fn with_nostr_client(mut self, client: Client) -> Self {
            self.nostr_client = Some(client);
            self
        }

        /// Use specific settings.
        pub fn with_settings(mut self, settings: Settings) -> Self {
            self.settings = Some(Arc::new(settings));
            self
        }

        /// Use a specific order message queue.
        pub fn with_order_msg_queue(mut self, queue: OrderMsgQueue) -> Self {
            self.order_msg_queue = Some(queue);
            self.mock_order_msg_queue = None;
            self
        }

        /// Attach a mock order-message queue helper for test assertions.
        pub fn with_mock_order_msg_queue(mut self, mock: MockOrderMsgQueue) -> Self {
            self.order_msg_queue = Some(mock.queue());
            self.mock_order_msg_queue = Some(mock);
            self
        }

        /// Use specific keys for tests.
        pub fn with_keys(mut self, keys: Keys) -> Self {
            self.keys = Some(keys);
            self
        }

        /// Build the test context.
        ///
        /// This is synchronous: callers must provide dependencies explicitly.
        /// The pool is required to avoid forcing async tests for pure logic.
        ///
        /// # Panics
        ///
        /// Panics if `with_pool()` or `with_settings()` were not called.
        pub fn build(self) -> AppContext {
            let pool = self
                .pool
                .expect("TestContextBuilder requires with_pool() for synchronous build");

            let nostr_client = self.nostr_client.unwrap_or_default();

            let settings = self
                .settings
                .expect("TestContextBuilder requires with_settings() — Settings has no Default");

            let order_msg_queue = self
                .order_msg_queue
                .unwrap_or_else(|| Arc::new(RwLock::new(Vec::new())));

            // Use provided keys or load from nsec_privkey_file
            let keys = self.keys.unwrap_or_else(|| {
                let nsec = std::fs::read_to_string(&settings.nostr.nsec_privkey_file)
                    .unwrap_or_else(|e| {
                        panic!(
                            "TestContextBuilder: failed to read nsec_privkey_file '{}': {}",
                            settings.nostr.nsec_privkey_file, e
                        )
                    });
                Keys::parse(nsec.trim())
                    .expect("TestContextBuilder: invalid nsec in nsec_privkey_file")
            });

            AppContext::new(pool, nostr_client, settings, order_msg_queue, keys)
        }

        /// Build context plus mock handles used for assertions.
        pub fn build_with_mocks(self) -> (AppContext, Option<MockOrderMsgQueue>) {
            let mock_order_msg_queue = self.mock_order_msg_queue.clone();
            let ctx = self.build();
            (ctx, mock_order_msg_queue)
        }
    }

    impl Default for TestContextBuilder {
        fn default() -> Self {
            Self::new()
        }
    }

    static TEST_KEY_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Generate deterministic test settings with sensible defaults.
    pub fn test_settings() -> Settings {
        let nsec_key = "nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd";
        let counter = TEST_KEY_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nsec_dir = std::env::temp_dir().join(format!(
            "mostro-test-{}-{}",
            std::process::id(),
            counter
        ));
        std::fs::create_dir_all(&nsec_dir).expect("failed to create test nsec key directory");
        let nsec_path = nsec_dir.join("nostr-key.nsec");
        std::fs::write(&nsec_path, nsec_key).expect("failed to write test nsec key file");

        Settings {
            database: DatabaseSettings {
                url: "sqlite::memory:".to_string(),
            },
            nostr: NostrSettings {
                nsec_privkey_file: nsec_path.to_string_lossy().to_string(),
                relays: vec!["wss://relay.test".to_string()],
                ..Default::default()
            },
            mostro: MostroSettings::default(),
            lightning: LightningSettings::default(),
            rpc: RpcSettings::default(),
            expiration: Some(ExpirationSettings::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_utils::{test_settings, MockOrderMsgQueue, TestContextBuilder};
    use sqlx::SqlitePool;
    use std::sync::Arc;

    #[tokio::test]
    async fn builder_can_inject_mock_order_message_queue() {
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        let mock_queue = MockOrderMsgQueue::new();

        let (ctx, mock_order_queue) = TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .with_mock_order_msg_queue(mock_queue)
            .build_with_mocks();

        let queue = mock_order_queue.expect("mock queue should be present");
        assert!(Arc::ptr_eq(&queue.queue(), ctx.order_msg_queue()));
    }

    #[tokio::test]
    async fn builder_can_use_custom_queue() {
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        let queue: super::OrderMsgQueue = Arc::new(tokio::sync::RwLock::new(Vec::new()));

        let ctx = TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .with_order_msg_queue(queue.clone())
            .build();

        assert!(Arc::ptr_eq(&queue, ctx.order_msg_queue()));
    }
}
