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
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tracing::info;

use super::admin::admin_service_server::AdminServiceServer;

/// Resolve `[rpc].listen_address` and `[rpc].port` into the address the server
/// binds.
///
/// `SocketAddr` only parses IP literals, with IPv6 bracketed. Hostnames such as
/// `localhost` and bare `::1` are therefore not bindable addresses, however
/// natural they look in a config file.
///
/// `config::util::validate_rpc_settings` calls this too, so a `listen_address`
/// that passes validation is guaranteed to be one `bind` can use: the two must
/// never disagree, or the daemon accepts a config at startup and then dies on
/// it.
pub(crate) fn listen_socket_addr(
    listen_address: &str,
    port: u16,
) -> Result<std::net::SocketAddr, String> {
    format!("{listen_address}:{port}")
        .parse::<std::net::SocketAddr>()
        .map_err(|e| {
            format!(
                "Invalid address {listen_address:?}: {e}. Expected an IP literal, with IPv6 \
                 bracketed — for example 127.0.0.1, [::1] or 0.0.0.0. Hostnames such as \
                 \"localhost\" are not resolved."
            )
        })
}

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

    /// Acquire the listener and return the bound address with the future that
    /// serves it.
    ///
    /// Everything that can fail on the way up happens here, before the caller
    /// gets anything to detach: a missing bearer token, unusable TLS material,
    /// an address already in use. The returned future only accepts connections,
    /// so a caller that awaits this function knows the admin API is listening
    /// and gated before it lets the rest of the daemon proceed — `[rpc].enabled
    /// = true` becomes an invariant rather than a hope.
    ///
    /// The address comes back from the listener rather than from the config, so
    /// it is the port actually in use: `port = 0` reports the ephemeral port the
    /// kernel picked instead of a literal `:0`.
    ///
    /// Refusing to serve without a token is deliberately redundant with
    /// `config::util::validate_rpc_settings`: a code path that reached here
    /// without one would expose an ungated admin API, and no RPC at all is the
    /// safer failure.
    pub fn bind(
        &self,
        my_keys: Keys,
        pool: Arc<Pool<Sqlite>>,
        ln_client: Arc<tokio::sync::Mutex<LndConnector>>,
    ) -> Result<
        (
            std::net::SocketAddr,
            impl std::future::Future<Output = Result<(), tonic::transport::Error>> + Send + 'static,
        ),
        Box<dyn std::error::Error>,
    > {
        let addr = listen_socket_addr(&self.listen_address, self.port)?;

        let token = read_rpc_token_env_var().ok_or_else(|| {
            format!("Refusing to start the admin RPC server: {RPC_TOKEN_ENV_VAR} is not set")
        })?;

        let admin_service = AdminServiceImpl::new(my_keys, pool, ln_client);

        let mut builder = Server::builder();
        let transport = match &self.tls {
            Some((cert_path, key_path)) => {
                let cert = std::fs::read(cert_path)
                    .map_err(|e| format!("Failed to read {cert_path}: {e}"))?;
                let key = std::fs::read(key_path)
                    .map_err(|e| format!("Failed to read {key_path}: {e}"))?;
                // Malformed PEM is rejected by `tls_config`, so it surfaces
                // here rather than inside the detached serving future.
                builder = builder
                    .tls_config(ServerTlsConfig::new().identity(Identity::from_pem(cert, key)))?;
                "TLS"
            }
            None => "plaintext",
        };

        // Binds eagerly: an occupied port is a startup error, not a surprise
        // discovered later by whoever happens to read the logs.
        let incoming =
            TcpIncoming::bind(addr).map_err(|e| format!("Failed to bind {addr}: {e}"))?;
        let bound = incoming
            .local_addr()
            .map_err(|e| format!("Failed to read the address bound to {addr}: {e}"))?;
        info!("RPC server listening on {} ({})", bound, transport);

        Ok((
            bound,
            builder
                .add_service(AdminServiceServer::with_interceptor(
                    admin_service,
                    BearerAuth::new(token),
                ))
                .serve_with_incoming(incoming),
        ))
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
    // serially. Async-aware because the guard is held across awaits.
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
    async fn bind_rejects_unparseable_address() {
        init_test_settings();
        let _lock = RPC_TOKEN_LOCK.lock().await;
        let _token = RpcTokenGuard::set(&"t".repeat(32));
        // `localhost` and bare `::1` are in here on purpose: they read like
        // valid loopback spellings, and `config::util` rejects them for exactly
        // this reason — `SocketAddr` cannot parse either.
        for address in ["not an address", "localhost", "::1"] {
            let server = server_at(address, 50051);
            let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
            let error = server
                .bind(Keys::generate(), Arc::new(pool), offline_ln_client().await)
                .err()
                .expect("an address that cannot be parsed must not serve");
            assert!(
                error.to_string().contains("Invalid address"),
                "{address} should have been refused, got: {error}"
            );
        }
    }

    #[test]
    fn listen_socket_addr_accepts_only_bindable_literals() {
        for address in ["127.0.0.1", "[::1]", "0.0.0.0", "[::]"] {
            assert!(
                listen_socket_addr(address, 50051).is_ok(),
                "{address} is a bindable literal"
            );
        }
        for address in ["localhost", "::1", "::", "mostro.example.com", ""] {
            assert!(
                listen_socket_addr(address, 50051).is_err(),
                "{address} is not a bindable literal"
            );
        }
    }

    /// The startup invariant the daemon depends on: a listener that cannot be
    /// acquired is reported by `bind` itself, so `main` learns about it before
    /// it detaches anything or continues booting.
    #[tokio::test]
    async fn bind_surfaces_bind_failure_before_returning() {
        init_test_settings();
        let _lock = RPC_TOKEN_LOCK.lock().await;
        let _token = RpcTokenGuard::set(&"t".repeat(32));
        // 8.8.8.8 is not a local interface, so the bind fails immediately.
        let server = server_at("8.8.8.8", 1);
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let error = server
            .bind(Keys::generate(), Arc::new(pool), offline_ln_client().await)
            .err()
            .expect("an unavailable address must fail before serving");
        assert!(error.to_string().contains("Failed to bind"));
    }

    #[tokio::test]
    async fn bind_refuses_to_serve_without_a_token() {
        init_test_settings();
        let _lock = RPC_TOKEN_LOCK.lock().await;
        let _token = RpcTokenGuard::unset();
        // 127.0.0.1:0 would otherwise bind successfully, so reaching the error
        // path proves the token check runs before anything starts listening.
        let server = server_at("127.0.0.1", 0);
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let error = server
            .bind(Keys::generate(), Arc::new(pool), offline_ln_client().await)
            .err()
            .expect("an admin RPC without a token must never serve");
        assert!(error.to_string().contains(RPC_TOKEN_ENV_VAR));
    }

    #[tokio::test]
    async fn bind_rejects_malformed_tls_material() {
        init_test_settings();
        let _lock = RPC_TOKEN_LOCK.lock().await;
        let _token = RpcTokenGuard::set(&"t".repeat(32));
        // Readable files that are not valid PEM: config validation accepts
        // them, so `bind` is the layer that has to catch this — and it must do
        // so before returning, or the daemon boots without the API it was told
        // to serve.
        let dir = std::env::temp_dir().join(format!("mostro-rpc-badtls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, b"not a certificate").expect("write cert");
        std::fs::write(&key, b"not a key").expect("write key");

        let server = RpcServer {
            listen_address: "127.0.0.1".to_string(),
            port: 0,
            tls: Some((
                cert.to_string_lossy().into_owned(),
                key.to_string_lossy().into_owned(),
            )),
        };
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        assert!(server
            .bind(Keys::generate(), Arc::new(pool), offline_ln_client().await)
            .is_err());
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
        use tonic::metadata::MetadataValue;
        use tonic::transport::Channel;
        use tonic::Request;

        init_test_settings();
        let _lock = RPC_TOKEN_LOCK.lock().await;
        let token = "t".repeat(32);
        let _guard = RpcTokenGuard::set(&token);

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let ln_client = offline_ln_client().await;
        // Port 0: the listener `bind` returns owns the ephemeral port for as
        // long as the test needs it. Reserving a port and releasing it first
        // would leave a window in which the kernel can hand it to another
        // process — a flake, not a failure of what is under test.
        let server = server_at("127.0.0.1", 0);
        let (bound, serving) = server
            .bind(Keys::generate(), Arc::new(pool), ln_client)
            .expect("bind must succeed on a free loopback port");
        let serving = tokio::spawn(serving);

        // No retry loop: `bind` returned, so the listener already exists and
        // the connection below must succeed on the first attempt. A retry here
        // would hide exactly the regression this asserts against.
        let channel = Channel::from_shared(format!("http://{bound}"))
            .expect("valid endpoint")
            .connect()
            .await
            .expect("the listener is open as soon as bind returns");

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
