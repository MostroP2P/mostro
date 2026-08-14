//! RPC server implementation for admin operations

use crate::config::constants::RPC_TOKEN_ENV_VAR;
use crate::config::secret::read_rpc_token_env_var;
use crate::config::settings::Settings;
use crate::lightning::LndConnector;
use crate::rpc::auth::BearerAuth;
use crate::rpc::service::AdminServiceImpl;
use nostr_sdk::prelude::Keys;
use sqlx::{Pool, Sqlite};
use std::sync::Arc;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tracing::{error, info};

use super::admin::admin_service_server::AdminServiceServer;

/// RPC server for admin operations
pub struct RpcServer {
    listen_address: String,
    port: u16,
    /// Certificate and key, or plaintext. Pairing them here keeps
    /// "both or neither" a property of the type rather than a runtime check.
    tls: Option<(String, String)>,
}

impl RpcServer {
    /// Create a new RPC server instance
    pub fn new() -> Self {
        let rpc_config = Settings::get_rpc();
        Self {
            listen_address: rpc_config.listen_address.clone(),
            port: rpc_config.port,
            tls: rpc_config
                .tls_paths()
                .map(|(cert, key)| (cert.to_string(), key.to_string())),
        }
    }

    /// Start the RPC server
    ///
    /// Refuses to serve without a bearer token. `validate_rpc_settings` already
    /// rejects that combination at startup, so this is the second lock on the
    /// same door: a code path that reached here without a token would expose an
    /// ungated admin API, and no RPC at all is the safer failure.
    pub async fn start(
        &self,
        my_keys: Keys,
        pool: Arc<Pool<Sqlite>>,
        ln_client: Arc<tokio::sync::Mutex<LndConnector>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let addr = format!("{}:{}", self.listen_address, self.port)
            .parse()
            .map_err(|e| format!("Invalid address: {}", e))?;

        let token = read_rpc_token_env_var().ok_or_else(|| {
            format!("Refusing to start the admin RPC server: {RPC_TOKEN_ENV_VAR} is not set")
        })?;

        let admin_service = AdminServiceImpl::new(my_keys, pool, ln_client);

        let mut builder = Server::builder();
        match &self.tls {
            Some((cert_path, key_path)) => {
                let cert = std::fs::read(cert_path)
                    .map_err(|e| format!("Failed to read {cert_path}: {e}"))?;
                let key = std::fs::read(key_path)
                    .map_err(|e| format!("Failed to read {key_path}: {e}"))?;
                builder = builder
                    .tls_config(ServerTlsConfig::new().identity(Identity::from_pem(cert, key)))?;
                info!("Starting RPC server on {} (TLS)", addr);
            }
            None => info!("Starting RPC server on {} (plaintext)", addr),
        }

        let server = builder
            .add_service(AdminServiceServer::with_interceptor(
                admin_service,
                BearerAuth::new(token),
            ))
            .serve(addr);

        if let Err(e) = server.await {
            error!("RPC server error: {}", e);
            return Err(Box::new(e));
        }

        Ok(())
    }

    /// Check if RPC server is enabled
    pub fn is_enabled() -> bool {
        Settings::get_rpc().enabled
    }
}

impl Default for RpcServer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::RpcSettings;

    #[test]
    fn test_rpc_settings_default() {
        let settings = RpcSettings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.listen_address, "127.0.0.1");
        assert_eq!(settings.port, 50051);
    }

    #[test]
    fn test_rpc_server_structure() {
        // Test that RpcServer can be created with explicit values
        let server = server_at("localhost", 8080);

        assert_eq!(server.listen_address, "localhost");
        assert_eq!(server.port, 8080);
    }

    #[test]
    fn test_address_formatting() {
        let server = server_at("127.0.0.1", 50051);

        let expected_addr = format!("{}:{}", server.listen_address, server.port);
        assert_eq!(expected_addr, "127.0.0.1:50051");
    }

    use crate::app::context::test_utils::test_settings;
    use crate::config::MOSTRO_CONFIG;
    use nostr_sdk::prelude::Keys;
    use std::sync::Arc;

    fn init_test_settings() {
        let _ = MOSTRO_CONFIG.set(test_settings());
    }

    /// Plaintext server bound to an explicit address, so the tests below stay
    /// readable as `RpcServer` grows optional fields.
    fn server_at(listen_address: &str, port: u16) -> RpcServer {
        RpcServer {
            listen_address: listen_address.to_string(),
            port,
            tls: None,
        }
    }

    // `MOSTRO_RPC_TOKEN` is process-wide state, so the tests that touch it run
    // serially. Async-aware because the guard is held across `start().await`.
    static RPC_TOKEN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Sets `MOSTRO_RPC_TOKEN` for the duration of a test and restores the
    /// previous value on drop.
    struct RpcTokenGuard {
        previous: Option<String>,
    }

    impl RpcTokenGuard {
        fn set(value: &str) -> Self {
            let previous = std::env::var(RPC_TOKEN_ENV_VAR).ok();
            std::env::set_var(RPC_TOKEN_ENV_VAR, value);
            Self { previous }
        }

        fn unset() -> Self {
            let previous = std::env::var(RPC_TOKEN_ENV_VAR).ok();
            std::env::remove_var(RPC_TOKEN_ENV_VAR);
            Self { previous }
        }
    }

    impl Drop for RpcTokenGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(RPC_TOKEN_ENV_VAR, value),
                None => std::env::remove_var(RPC_TOKEN_ENV_VAR),
            }
        }
    }

    /// Offline `LndConnector` (lazy connect, no network until first RPC).
    async fn offline_ln_client() -> Arc<tokio::sync::Mutex<LndConnector>> {
        let dir = std::env::temp_dir().join(format!("mostro-rpcsrv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cert = dir.join("tls.cert");
        let macaroon = dir.join("admin.macaroon");
        std::fs::write(&cert, b"").expect("write cert");
        std::fs::write(&macaroon, b"").expect("write macaroon");
        let client = fedimint_tonic_lnd::connect("https://127.0.0.1:1".to_string(), cert, macaroon)
            .await
            .expect("lazy connect must not touch the network");
        Arc::new(tokio::sync::Mutex::new(LndConnector { client }))
    }

    #[test]
    fn new_reads_settings_and_default_delegates() {
        init_test_settings();
        let server = RpcServer::new();
        let rpc = Settings::get_rpc();
        assert_eq!(server.listen_address, rpc.listen_address);
        assert_eq!(server.port, rpc.port);

        let defaulted = RpcServer::default();
        assert_eq!(defaulted.listen_address, server.listen_address);
        assert_eq!(defaulted.port, server.port);
    }

    #[test]
    fn is_enabled_reflects_settings() {
        init_test_settings();
        // Canonical test settings keep the RPC server disabled.
        assert!(!RpcServer::is_enabled());
    }

    #[tokio::test]
    async fn start_rejects_unparseable_address() {
        init_test_settings();
        let _lock = RPC_TOKEN_LOCK.lock().await;
        let _token = RpcTokenGuard::set(&"t".repeat(32));
        let server = server_at("not an address", 50051);
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let result = server
            .start(Keys::generate(), Arc::new(pool), offline_ln_client().await)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn start_surfaces_bind_failure() {
        init_test_settings();
        let _lock = RPC_TOKEN_LOCK.lock().await;
        let _token = RpcTokenGuard::set(&"t".repeat(32));
        // 8.8.8.8 is not a local interface: the bind fails immediately, so
        // the server error path is exercised without serving traffic.
        let server = server_at("8.8.8.8", 1);
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let result = server
            .start(Keys::generate(), Arc::new(pool), offline_ln_client().await)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn start_refuses_to_serve_without_a_token() {
        init_test_settings();
        let _lock = RPC_TOKEN_LOCK.lock().await;
        let _token = RpcTokenGuard::unset();
        // 127.0.0.1:0 would otherwise bind successfully and serve forever, so
        // reaching the error path proves the token check ran before the bind.
        let server = server_at("127.0.0.1", 0);
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let error = server
            .start(Keys::generate(), Arc::new(pool), offline_ln_client().await)
            .await
            .expect_err("an admin RPC without a token must never serve");
        assert!(error.to_string().contains(RPC_TOKEN_ENV_VAR));
    }

    /// End-to-end proof that the interceptor gates the service that is actually
    /// served. The unit tests in `crate::rpc::auth` only cover the interceptor
    /// in isolation, so they would stay green if a refactor registered the
    /// service without it — which is precisely the regression that would
    /// reopen the hole this module exists to close.
    ///
    /// `GetVersion` is the probe: it touches neither the database nor LND, so
    /// the only thing under test is the authentication decision.
    #[tokio::test]
    async fn served_rpc_rejects_calls_without_the_token() {
        use crate::rpc::admin::{admin_service_client::AdminServiceClient, GetVersionRequest};
        use std::time::Duration;
        use tonic::metadata::MetadataValue;
        use tonic::transport::Channel;
        use tonic::Request;

        init_test_settings();
        let _lock = RPC_TOKEN_LOCK.lock().await;
        let token = "t".repeat(32);
        let _guard = RpcTokenGuard::set(&token);

        // Reserve an ephemeral port and release it: `serve` takes an address,
        // not a listener, and a fixed port would collide across parallel runs.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("reserve an ephemeral port")
            .local_addr()
            .expect("reserved address")
            .port();

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let ln_client = offline_ln_client().await;
        let server = server_at("127.0.0.1", port);
        let serving = tokio::spawn(async move {
            let _ = server
                .start(Keys::generate(), Arc::new(pool), ln_client)
                .await;
        });

        let endpoint = format!("http://127.0.0.1:{port}");
        let mut channel = None;
        for _ in 0..100 {
            match Channel::from_shared(endpoint.clone())
                .expect("valid endpoint")
                .connect()
                .await
            {
                Ok(connected) => {
                    channel = Some(connected);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        let channel = channel.expect("the RPC server should accept connections");

        let status = AdminServiceClient::new(channel.clone())
            .get_version(GetVersionRequest {})
            .await
            .expect_err("an anonymous call must be refused");
        assert_eq!(status.code(), tonic::Code::Unauthenticated);

        let credential: MetadataValue<_> = format!("Bearer {token}")
            .parse()
            .expect("token is a valid header value");
        let mut authenticated =
            AdminServiceClient::with_interceptor(channel, move |mut request: Request<()>| {
                request
                    .metadata_mut()
                    .insert("authorization", credential.clone());
                Ok(request)
            });
        let version = authenticated
            .get_version(GetVersionRequest {})
            .await
            .expect("an authenticated call must go through")
            .into_inner()
            .version;
        assert_eq!(version, env!("CARGO_PKG_VERSION"));

        serving.abort();
    }

    #[test]
    fn test_default_rpc_settings() {
        let default_settings = RpcSettings::default();

        // Test that defaults are sensible
        assert!(
            !default_settings.enabled,
            "RPC should be disabled by default"
        );
        assert!(
            !default_settings.listen_address.is_empty(),
            "Listen address should not be empty"
        );
        assert!(default_settings.port > 0, "Port should be positive");
        // Note: u16 max is 65535, so any u16 is valid by definition
    }
}
