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
    nostr_client: Arc<Client>,
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
        let nostr_client = Arc::new(get_nostr_client()?.clone());
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
        nostr_client: Arc<Client>,
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
        nostr_client: Option<Arc<Client>>,
        settings: Option<Arc<Settings>>,
    }

    impl TestContextBuilder {
        pub fn new() -> Self {
            Self {
                pool: None,
                nostr_client: None,
                settings: None,
            }
        }

        /// Use a specific database pool (e.g., in-memory SQLite for tests).
        pub fn with_pool(mut self, pool: Arc<Pool<Sqlite>>) -> Self {
            self.pool = Some(pool);
            self
        }

        /// Use a specific Nostr client (e.g., a mock or test-configured client).
        pub fn with_nostr_client(mut self, client: Arc<Client>) -> Self {
            self.nostr_client = Some(client);
            self
        }

        /// Use specific settings.
        pub fn with_settings(mut self, settings: Settings) -> Self {
            self.settings = Some(Arc::new(settings));
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

            let nostr_client = self
                .nostr_client
                .unwrap_or_else(|| Arc::new(Client::default()));

            let settings = self
                .settings
                .expect("TestContextBuilder requires with_settings() — Settings has no Default");

            let order_msg_queue = Arc::new(RwLock::new(Vec::new()));

            AppContext::new(pool, nostr_client, settings, order_msg_queue)
        }
    }

    impl Default for TestContextBuilder {
        fn default() -> Self {
            Self::new()
        }
    }
}
