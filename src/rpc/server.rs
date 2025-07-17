//! RPC server implementation for admin operations

use crate::config::settings::Settings;
use crate::lightning::LndConnector;
use crate::rpc::service::AdminServiceImpl;
use nostr_sdk::Keys;
use sqlx::{Pool, Sqlite};
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
    ) -> Result<(), Box<dyn std::error::Error>> {
        let addr = format!("{}:{}", self.listen_address, self.port)
            .parse()
            .map_err(|e| format!("Invalid address: {}", e))?;

        let admin_service = AdminServiceImpl::new(my_keys, pool, ln_client);

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
