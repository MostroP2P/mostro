//! RPC server implementation for admin operations

use crate::app::maintenance::MaintenanceState;
use crate::config::settings::Settings;
use crate::lightning::LndConnector;
use crate::rpc::service::AdminServiceImpl;
use nostr_sdk::prelude::Keys;
use sqlx::{Pool, Sqlite};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tonic::transport::Server;
use tracing::{error, info};

use super::admin::admin_service_server::AdminServiceServer;

/// RPC server for admin operations
pub struct RpcServer {
    listen_address: String,
    port: u16,
}

impl RpcServer {
    /// Create a new RPC server instance
    pub fn new() -> Self {
        let rpc_config = Settings::get_rpc();
        Self {
            listen_address: rpc_config.listen_address.clone(),
            port: rpc_config.port,
        }
    }

    /// Start the RPC server
    pub async fn start(
        &self,
        my_keys: Keys,
        pool: Arc<Pool<Sqlite>>,
        ln_client: Arc<tokio::sync::Mutex<LndConnector>>,
        maintenance: MaintenanceState,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let addr = listen_socket_addr(&self.listen_address, self.port)?;

        let admin_service = AdminServiceImpl::new(my_keys, pool, ln_client, maintenance);

        info!("Starting RPC server on {}", addr);

        let server = Server::builder()
            .add_service(AdminServiceServer::new(admin_service))
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

/// Build the bind address from `[rpc].listen_address` + `port`.
///
/// `listen_address` must be an IP literal (`Server::serve` needs a
/// `SocketAddr`, so hostnames never worked). Going through `IpAddr` instead
/// of `format!("{}:{}")` is what makes IPv6 work: `"::1:50051"` does not
/// parse, `[::1]:50051` does.
pub fn listen_socket_addr(listen_address: &str, port: u16) -> Result<SocketAddr, String> {
    let ip: IpAddr = listen_address
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|e| format!("Invalid [rpc].listen_address {listen_address:?}: {e}"))?;
    Ok(SocketAddr::new(ip, port))
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
        let server = RpcServer {
            listen_address: "localhost".to_string(),
            port: 8080,
        };

        assert_eq!(server.listen_address, "localhost");
        assert_eq!(server.port, 8080);
    }

    #[test]
    fn test_address_formatting() {
        let server = RpcServer {
            listen_address: "127.0.0.1".to_string(),
            port: 50051,
        };

        let expected_addr = format!("{}:{}", server.listen_address, server.port);
        assert_eq!(expected_addr, "127.0.0.1:50051");
    }

    #[test]
    fn listen_socket_addr_handles_ipv4_ipv6_and_rejects_garbage() {
        assert_eq!(
            listen_socket_addr("127.0.0.1", 50051).unwrap().to_string(),
            "127.0.0.1:50051"
        );
        assert_eq!(
            listen_socket_addr("::1", 50051).unwrap().to_string(),
            "[::1]:50051"
        );
        assert_eq!(
            listen_socket_addr("[::1]", 50051).unwrap().to_string(),
            "[::1]:50051"
        );
        assert!(listen_socket_addr("localhost", 50051).is_err());
        assert!(listen_socket_addr("not an address", 50051).is_err());
    }

    /// `::1` used to be formatted as `::1:50051`, which `SocketAddr` rejects.
    /// Binding it now works (the server is dropped right after the bind).
    #[tokio::test]
    async fn start_accepts_ipv6_loopback_listener() {
        init_test_settings();
        let server = RpcServer {
            listen_address: "::1".to_string(),
            port: 0,
        };
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let start = server.start(
            Keys::generate(),
            Arc::new(pool),
            offline_ln_client().await,
            MaintenanceState::new(),
        );
        // A successful bind keeps serving forever; a failed one returns
        // immediately. Give it a moment and treat "still serving" as success.
        match tokio::time::timeout(std::time::Duration::from_millis(500), start).await {
            Err(_elapsed) => {}
            Ok(result) => {
                // Only a missing IPv6 loopback on the host may fail the bind.
                let msg = result.unwrap_err().to_string();
                assert!(
                    !msg.contains("Invalid [rpc].listen_address"),
                    "::1 must parse: {msg}"
                );
            }
        }
    }

    use crate::app::context::test_utils::test_settings;
    use crate::config::MOSTRO_CONFIG;
    use nostr_sdk::prelude::Keys;
    use std::sync::Arc;

    fn init_test_settings() {
        let _ = MOSTRO_CONFIG.set(test_settings());
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
        let server = RpcServer {
            listen_address: "not an address".to_string(),
            port: 50051,
        };
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let result = server
            .start(
                Keys::generate(),
                Arc::new(pool),
                offline_ln_client().await,
                MaintenanceState::new(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn start_surfaces_bind_failure() {
        init_test_settings();
        // 8.8.8.8 is not a local interface: the bind fails immediately, so
        // the server error path is exercised without serving traffic.
        let server = RpcServer {
            listen_address: "8.8.8.8".to_string(),
            port: 1,
        };
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let result = server
            .start(
                Keys::generate(),
                Arc::new(pool),
                offline_ln_client().await,
                MaintenanceState::new(),
            )
            .await;
        assert!(result.is_err());
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
