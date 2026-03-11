//! Application context for dependency injection.
//!
//! `AppContext` holds all shared dependencies needed by handler functions,
//! replacing direct access to global state (`get_db_pool()`, `get_nostr_client()`,
//! `Settings::get_mostro()`, etc.).
//!
//! This enables unit testing with mock implementations — see `TestContextBuilder`.

use crate::config::settings::Settings;
use crate::config::MESSAGE_QUEUES;
use mostro_core::prelude::Message;
use nostr_sdk::{Client, PublicKey};
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
}

impl AppContext {
    /// Build an `AppContext` from the current global state.
    ///
    /// This is the bridge between the old global-based architecture and the
    /// new DI-based one. Once all handlers accept `&AppContext`, the globals
    /// can be removed and this method replaced with explicit construction.
    pub fn from_globals() -> Result<Self, mostro_core::prelude::MostroError> {
        use crate::config::settings::get_db_pool;
        use crate::config::MOSTRO_CONFIG;
        use crate::util::get_nostr_client;
        use mostro_core::prelude::{MostroError::MostroInternalErr, ServiceError};

        let pool = get_db_pool();
        let nostr_client = get_nostr_client()?.clone();
        let settings = Arc::new(
            MOSTRO_CONFIG
                .get()
                .ok_or_else(|| {
                    MostroInternalErr(ServiceError::UnexpectedError(
                        "MOSTRO_CONFIG not initialized".to_string(),
                    ))
                })?
                .clone(),
        );
        let order_msg_queue = MESSAGE_QUEUES.queue_order_msg.clone();

        Ok(Self::new(pool, nostr_client, settings, order_msg_queue))
    }

    /// Create a new application context.
    pub fn new(
        pool: Arc<Pool<Sqlite>>,
        nostr_client: Client,
        settings: Arc<Settings>,
        order_msg_queue: OrderMsgQueue,
    ) -> Self {
        Self {
            pool,
            nostr_client,
            settings,
            order_msg_queue,
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
}

#[cfg(test)]
pub mod test_utils {
    use super::*;
    use crate::config::types::{
        DatabaseSettings, ExpirationSettings, LightningSettings, MostroSettings, NostrSettings,
        RpcSettings,
    };

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
    #[derive(Debug, Clone)]
    pub struct MockNostrClient {
        pub published_event_ids: Arc<RwLock<Vec<String>>>,
        pub connected_relays: Arc<RwLock<Vec<String>>>,
    }

    impl MockNostrClient {
        pub fn new() -> Self {
            Self {
                published_event_ids: Arc::new(RwLock::new(Vec::new())),
                connected_relays: Arc::new(RwLock::new(Vec::new())),
            }
        }

        pub async fn record_published_event(&self, event_id: impl Into<String>) {
            self.published_event_ids.write().await.push(event_id.into());
        }

        pub async fn record_connected_relay(&self, relay: impl Into<String>) {
            self.connected_relays.write().await.push(relay.into());
        }
    }

    impl Default for MockNostrClient {
        fn default() -> Self {
            Self::new()
        }
    }

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

    pub struct TestContextBuilder {
        pool: Option<Arc<Pool<Sqlite>>>,
        nostr_client: Option<Client>,
        settings: Option<Arc<Settings>>,
        order_msg_queue: Option<OrderMsgQueue>,
        mock_nostr_client: Option<MockNostrClient>,
        mock_order_msg_queue: Option<MockOrderMsgQueue>,
    }

    impl TestContextBuilder {
        pub fn new() -> Self {
            Self {
                pool: None,
                nostr_client: None,
                settings: None,
                order_msg_queue: None,
                mock_nostr_client: None,
                mock_order_msg_queue: None,
            }
        }

        /// Use a specific database pool (e.g., in-memory SQLite for tests).
        pub fn with_pool(mut self, pool: Arc<Pool<Sqlite>>) -> Self {
            self.pool = Some(pool);
            self
        }

        /// Use a specific Nostr client (e.g., a mock or test-configured client).
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
            self
        }

        /// Attach a mock Nostr client helper for test assertions.
        ///
        /// This does not replace `nostr_sdk::Client` behavior directly; it provides
        /// an explicit test-side recorder/state holder that tests can use while
        /// migrating handlers away from globals.
        pub fn with_mock_nostr_client(mut self, mock: MockNostrClient) -> Self {
            self.mock_nostr_client = Some(mock);
            self
        }

        /// Attach a mock order-message queue helper for test assertions.
        pub fn with_mock_order_msg_queue(mut self, mock: MockOrderMsgQueue) -> Self {
            self.order_msg_queue = Some(mock.queue());
            self.mock_order_msg_queue = Some(mock);
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

            AppContext::new(pool, nostr_client, settings, order_msg_queue)
        }

        /// Build context plus mock handles used for assertions.
        pub fn build_with_mocks(
            self,
        ) -> (
            AppContext,
            Option<MockNostrClient>,
            Option<MockOrderMsgQueue>,
        ) {
            let mock_nostr_client = self.mock_nostr_client.clone();
            let mock_order_msg_queue = self.mock_order_msg_queue.clone();
            let ctx = self.build();
            (ctx, mock_nostr_client, mock_order_msg_queue)
        }
    }

    impl Default for TestContextBuilder {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Generate deterministic test settings with sensible defaults.
    pub fn test_settings() -> Settings {
        Settings {
            database: DatabaseSettings {
                url: "sqlite::memory:".to_string(),
            },
            nostr: NostrSettings {
                nsec_privkey: "nsec_test_placeholder".to_string(),
                relays: vec!["wss://relay.test".to_string()],
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

        let (ctx, _mock_nostr, mock_order_queue) = TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .with_mock_order_msg_queue(mock_queue)
            .build_with_mocks();

        let queue = mock_order_queue.expect("mock queue should be present");
        assert_eq!(queue.len().await, 0);
        assert_eq!(ctx.order_msg_queue().read().await.len(), 0);
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
