//! RPC service implementation for admin operations

use crate::app::daemon_state;
use crate::app::maintenance::{
    drain_counters, MaintenanceState, KEY_LN_NODE_PUBKEY, KEY_REASON, KEY_SINCE,
};
use crate::config::settings::Settings;
use crate::config::LN_STATUS;

use crate::lightning::LndConnector;
use crate::rpc::admin::{
    admin_service_server::AdminService, AddSolverRequest, AddSolverResponse, CancelOrderRequest,
    CancelOrderResponse, DrainCounters, GetMaintenanceStatusRequest, GetMaintenanceStatusResponse,
    SetMaintenanceModeRequest, SetMaintenanceModeResponse, SettleOrderRequest, SettleOrderResponse,
    TakeDisputeRequest, TakeDisputeResponse, ValidateDbPasswordRequest, ValidateDbPasswordResponse,
};
use crate::rpc::rate_limiter::RateLimiter;
use mostro_core::nip59::UnwrappedMessage;
use nostr_sdk::prelude::Keys;
use secrecy::{ExposeSecret, SecretString};
use sqlx::{Pool, Sqlite};
use std::sync::Arc;
use std::time::Duration;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

/// Implementation of the AdminService gRPC service
pub struct AdminServiceImpl {
    keys: Keys,
    pool: Arc<Pool<Sqlite>>,
    ln_client: Arc<tokio::sync::Mutex<LndConnector>>,
    password_rate_limiter: Arc<RateLimiter>,
    /// Same clone the event loop's `AppContext` holds, so a flip here is
    /// observed by the escrow gate immediately.
    maintenance: MaintenanceState,
    /// `[rpc].auth_token`. When set, every mutating RPC requires
    /// `authorization: Bearer <token>`.
    auth_token: Option<SecretString>,
}

impl AdminServiceImpl {
    pub fn new(
        keys: Keys,
        pool: Arc<Pool<Sqlite>>,
        ln_client: Arc<tokio::sync::Mutex<LndConnector>>,
        maintenance: MaintenanceState,
    ) -> Self {
        let retention_secs = Settings::get_rpc().rate_limiter_stale_duration;
        Self {
            keys,
            pool,
            ln_client,
            password_rate_limiter: Arc::new(RateLimiter::new(Duration::from_secs(retention_secs))),
            maintenance,
            auth_token: Settings::get_rpc().auth_token.clone(),
        }
    }

    /// Application-layer check for the mutating RPCs. A no-op unless
    /// `[rpc].auth_token` is configured; then the request must carry
    /// `authorization: Bearer <token>` and it is compared in constant time.
    /// This is what protects the book when the port is reached through a
    /// forwarder (SSH tunnel, sidecar, reverse proxy) whose connection looks
    /// like a loopback peer.
    fn require_auth<T>(&self, request: &Request<T>, rpc: &str) -> Result<(), Status> {
        let Some(expected) = &self.auth_token else {
            return Ok(());
        };
        let presented = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        match presented {
            Some(token)
                if constant_time_eq(token.as_bytes(), expected.expose_secret().as_bytes()) =>
            {
                Ok(())
            }
            _ => {
                warn!("{rpc} refused: missing or invalid bearer token");
                Err(Status::permission_denied(
                    "missing or invalid authorization bearer token",
                ))
            }
        }
    }

    /// The admin service has no authentication interceptor; mutating the
    /// maintenance flag is restricted to loopback peers so a non-loopback
    /// `[rpc].listen_address` or a forwarded port cannot close or reopen the
    /// book. Missing peer info is refused too (same as `validate_db_password`).
    fn require_loopback<T>(request: &Request<T>) -> Result<std::net::IpAddr, Status> {
        let ip = request
            .remote_addr()
            .ok_or_else(|| Status::internal("Unable to determine client address"))?
            .ip();
        if ip.is_loopback() {
            Ok(ip)
        } else {
            warn!("SetMaintenanceMode refused for non-loopback peer {ip}");
            Err(Status::permission_denied(
                "SetMaintenanceMode is only accepted from loopback peers",
            ))
        }
    }
}

/// Length-independent byte comparison: the whole input is always scanned.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for (x, y) in a.iter().zip(b.iter().cycle()) {
        diff |= x ^ y;
    }
    diff == 0 && !b.is_empty()
}

impl AdminServiceImpl {
    /// Convert admin actions to use existing handlers
    /// This creates the necessary structures to call existing admin handlers
    async fn call_admin_cancel(
        &self,
        order_id: String,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::app::admin_cancel::admin_cancel_action;
        use mostro_core::message::{Action, Message};
        use nostr_sdk::prelude::Timestamp;
        use uuid::Uuid;

        // Create a mock message for the admin cancel action
        let msg = Message::new_order(
            Some(Uuid::parse_str(&order_id)?),
            request_id.map(|id| id.parse().unwrap_or(1)),
            None,
            Action::AdminCancel,
            None,
        );

        // Admin RPC flows synthesize the inbound event with the node's own
        // pubkey in both `identity` and `sender` slots. Authorization is then
        // enforced downstream: the caller must be the assigned solver
        // (`is_assigned_solver`), with `ensure_dispute_finalize_permission`
        // bypassing solver category checks for the daemon key (same as
        // `admin_take_dispute`). gRPC transport authenticates the operator.
        let event = UnwrappedMessage {
            message: msg.clone(),
            signature: None,
            sender: self.keys.public_key(),
            identity: self.keys.public_key(),
            created_at: Timestamp::now(),
        };

        use crate::app::context::AppContext;
        use crate::config::MESSAGE_QUEUES;
        use crate::config::MOSTRO_CONFIG;
        use crate::util::get_nostr_client;

        let nostr_client = get_nostr_client()
            .map_err(|e| format!("Failed to get Nostr client: {}", e))?
            .clone();
        let settings = std::sync::Arc::new(
            MOSTRO_CONFIG
                .get()
                .ok_or_else(|| "MOSTRO_CONFIG not initialized".to_string())?
                .clone(),
        );
        let ctx = AppContext::new(
            self.pool.clone(),
            nostr_client,
            settings,
            MESSAGE_QUEUES.queue_order_msg.clone(),
            self.keys.clone(),
        );
        let mut ln_client = self.ln_client.lock().await;
        admin_cancel_action(&ctx, msg, &event, &self.keys, &mut ln_client)
            .await
            .map_err(|e| format!("Admin cancel failed: {}", e))?;

        Ok(())
    }

    /// `CancelOrderRequest.pretrade_only`: refuse anything that is not
    /// still `pending` / `waiting-taker-bond`, so operator tooling that
    /// means "cancel this pending order" can never resolve a dispute by a
    /// mistyped id. Returns the operator-facing reason on refusal.
    async fn ensure_pretrade(&self, order_id: &str) -> Result<(), String> {
        use mostro_core::db::Crud;
        use mostro_core::order::{Order, Status as OrderStatus};
        let id = uuid::Uuid::parse_str(order_id).map_err(|e| format!("invalid order id: {e}"))?;
        let order = Order::by_id(self.pool.as_ref(), id)
            .await
            .map_err(|e| format!("order lookup failed: {e}"))?
            .ok_or_else(|| format!("order {order_id} not found"))?;
        if order.check_status(OrderStatus::Pending).is_ok()
            || order.check_status(OrderStatus::WaitingTakerBond).is_ok()
        {
            return Ok(());
        }
        // The dispute-flow hint is only right when the order really is in
        // dispute; for any other status that flow would be refused too.
        let hint = if order.check_status(OrderStatus::Dispute).is_ok() {
            "; use the dispute flow (AdminCancel / AdminSettle) instead"
        } else {
            ""
        };
        Err(format!(
            "order {order_id} is {} and pretrade_only was requested{hint}",
            order.status
        ))
    }

    async fn call_admin_settle(
        &self,
        order_id: String,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::app::admin_settle::admin_settle_action;
        use mostro_core::message::{Action, Message};
        use nostr_sdk::prelude::Timestamp;
        use uuid::Uuid;

        let msg = Message::new_order(
            Some(Uuid::parse_str(&order_id)?),
            request_id.and_then(|id| id.parse::<u64>().ok()),
            None,
            Action::AdminSettle,
            None,
        );

        let event = UnwrappedMessage {
            message: msg.clone(),
            signature: None,
            sender: self.keys.public_key(),
            identity: self.keys.public_key(),
            created_at: Timestamp::now(),
        };

        use crate::app::context::AppContext;
        use crate::config::MESSAGE_QUEUES;
        use crate::config::MOSTRO_CONFIG;
        use crate::util::get_nostr_client;

        let nostr_client = get_nostr_client()
            .map_err(|e| format!("Failed to get Nostr client: {}", e))?
            .clone();
        let settings = std::sync::Arc::new(
            MOSTRO_CONFIG
                .get()
                .ok_or_else(|| "MOSTRO_CONFIG not initialized".to_string())?
                .clone(),
        );
        let ctx = AppContext::new(
            self.pool.clone(),
            nostr_client,
            settings,
            MESSAGE_QUEUES.queue_order_msg.clone(),
            self.keys.clone(),
        );
        let mut ln_client = self.ln_client.lock().await;
        admin_settle_action(&ctx, msg, &event, &self.keys, &mut ln_client)
            .await
            .map_err(|e| format!("Admin settle failed: {}", e))?;

        Ok(())
    }

    async fn call_admin_add_solver(
        &self,
        solver_pubkey: String,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::app::admin_add_solver::admin_add_solver_action;
        use mostro_core::message::{Action, Message, Payload};
        use nostr_sdk::prelude::Timestamp;

        let msg = Message::new_dispute(
            None,
            request_id.and_then(|id| id.parse::<u64>().ok()),
            None,
            Action::AdminAddSolver,
            Some(Payload::TextMessage(solver_pubkey)),
        );

        let event = UnwrappedMessage {
            message: msg.clone(),
            signature: None,
            sender: self.keys.public_key(),
            identity: self.keys.public_key(),
            created_at: Timestamp::now(),
        };

        use crate::app::context::AppContext;
        use crate::config::MESSAGE_QUEUES;
        use crate::config::MOSTRO_CONFIG;
        use crate::util::get_nostr_client;

        let nostr_client = get_nostr_client()
            .map_err(|e| format!("Failed to get Nostr client: {}", e))?
            .clone();
        let settings = std::sync::Arc::new(
            MOSTRO_CONFIG
                .get()
                .ok_or_else(|| "MOSTRO_CONFIG not initialized".to_string())?
                .clone(),
        );
        let ctx = AppContext::new(
            self.pool.clone(),
            nostr_client,
            settings,
            MESSAGE_QUEUES.queue_order_msg.clone(),
            self.keys.clone(),
        );
        admin_add_solver_action(&ctx, msg, &event, &self.keys)
            .await
            .map_err(|e| format!("Admin add solver failed: {}", e))?;

        Ok(())
    }

    async fn call_admin_take_dispute(
        &self,
        dispute_id: String,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::app::admin_take_dispute::admin_take_dispute_action;
        use mostro_core::message::{Action, Message};
        use nostr_sdk::prelude::Timestamp;
        use uuid::Uuid;

        let msg = Message::new_dispute(
            Some(Uuid::parse_str(&dispute_id)?),
            request_id.and_then(|id| id.parse::<u64>().ok()),
            None,
            Action::AdminTakeDispute,
            None,
        );

        let event = UnwrappedMessage {
            message: msg.clone(),
            signature: None,
            sender: self.keys.public_key(),
            identity: self.keys.public_key(),
            created_at: Timestamp::now(),
        };

        use crate::app::context::AppContext;
        use crate::config::MESSAGE_QUEUES;
        use crate::config::MOSTRO_CONFIG;
        use crate::util::get_nostr_client;

        let nostr_client = get_nostr_client()
            .map_err(|e| format!("Failed to get Nostr client: {}", e))?
            .clone();
        let settings = std::sync::Arc::new(
            MOSTRO_CONFIG
                .get()
                .ok_or_else(|| "MOSTRO_CONFIG not initialized".to_string())?
                .clone(),
        );
        let ctx = AppContext::new(
            self.pool.clone(),
            nostr_client,
            settings,
            MESSAGE_QUEUES.queue_order_msg.clone(),
            self.keys.clone(),
        );
        admin_take_dispute_action(&ctx, msg, &event, &self.keys)
            .await
            .map_err(|e| format!("Admin take dispute failed: {}", e))?;

        Ok(())
    }
}

#[tonic::async_trait]
impl AdminService for AdminServiceImpl {
    async fn cancel_order(
        &self,
        request: Request<CancelOrderRequest>,
    ) -> Result<Response<CancelOrderResponse>, Status> {
        self.require_auth(&request, "CancelOrder")?;
        let req = request.into_inner();
        info!("Received cancel order request for order: {}", req.order_id);

        if req.pretrade_only.unwrap_or(false) {
            if let Err(msg) = self.ensure_pretrade(&req.order_id).await {
                warn!("CancelOrder refused: {msg}");
                return Ok(Response::new(CancelOrderResponse {
                    success: false,
                    error_message: Some(msg),
                }));
            }
        }

        match self.call_admin_cancel(req.order_id, req.request_id).await {
            Ok(()) => Ok(Response::new(CancelOrderResponse {
                success: true,
                error_message: None,
            })),
            Err(e) => {
                error!("Cancel order failed: {}", e);
                Ok(Response::new(CancelOrderResponse {
                    success: false,
                    error_message: Some(e.to_string()),
                }))
            }
        }
    }

    async fn settle_order(
        &self,
        request: Request<SettleOrderRequest>,
    ) -> Result<Response<SettleOrderResponse>, Status> {
        self.require_auth(&request, "SettleOrder")?;
        let req = request.into_inner();
        info!("Received settle order request for order: {}", req.order_id);

        match self.call_admin_settle(req.order_id, req.request_id).await {
            Ok(()) => Ok(Response::new(SettleOrderResponse {
                success: true,
                error_message: None,
            })),
            Err(e) => {
                error!("Settle order failed: {}", e);
                Ok(Response::new(SettleOrderResponse {
                    success: false,
                    error_message: Some(e.to_string()),
                }))
            }
        }
    }

    async fn add_solver(
        &self,
        request: Request<AddSolverRequest>,
    ) -> Result<Response<AddSolverResponse>, Status> {
        self.require_auth(&request, "AddSolver")?;
        let req = request.into_inner();
        info!(
            "Received add solver request for pubkey: {}",
            req.solver_pubkey
        );

        match self
            .call_admin_add_solver(req.solver_pubkey, req.request_id)
            .await
        {
            Ok(()) => Ok(Response::new(AddSolverResponse {
                success: true,
                error_message: None,
            })),
            Err(e) => {
                error!("Add solver failed: {}", e);
                Ok(Response::new(AddSolverResponse {
                    success: false,
                    error_message: Some(e.to_string()),
                }))
            }
        }
    }

    async fn take_dispute(
        &self,
        request: Request<TakeDisputeRequest>,
    ) -> Result<Response<TakeDisputeResponse>, Status> {
        self.require_auth(&request, "TakeDispute")?;
        let req = request.into_inner();
        info!(
            "Received take dispute request for dispute: {}",
            req.dispute_id
        );

        match self
            .call_admin_take_dispute(req.dispute_id, req.request_id)
            .await
        {
            Ok(()) => Ok(Response::new(TakeDisputeResponse {
                success: true,
                error_message: None,
            })),
            Err(e) => {
                error!("Take dispute failed: {}", e);
                Ok(Response::new(TakeDisputeResponse {
                    success: false,
                    error_message: Some(e.to_string()),
                }))
            }
        }
    }

    async fn get_version(
        &self,
        _request: Request<crate::rpc::admin::GetVersionRequest>,
    ) -> Result<Response<crate::rpc::admin::GetVersionResponse>, Status> {
        let version = env!("CARGO_PKG_VERSION").to_string();
        Ok(Response::new(crate::rpc::admin::GetVersionResponse {
            version,
        }))
    }

    async fn set_maintenance_mode(
        &self,
        request: Request<SetMaintenanceModeRequest>,
    ) -> Result<Response<SetMaintenanceModeResponse>, Status> {
        let ip = Self::require_loopback(&request)?;
        self.require_auth(&request, "SetMaintenanceMode")?;
        let req = request.into_inner();
        info!(
            "Received SetMaintenanceMode(enabled={}) from {ip}, request_id: {:?}",
            req.enabled, req.request_id
        );
        match self
            .maintenance
            .set(&self.pool, req.enabled, req.reason.as_deref())
            .await
        {
            Ok(()) => {
                warn!(
                    "Maintenance mode is now {}",
                    if req.enabled { "ON" } else { "OFF" }
                );
                Ok(Response::new(SetMaintenanceModeResponse {
                    success: true,
                    error_message: None,
                }))
            }
            Err(e) => {
                error!("SetMaintenanceMode failed: {e}");
                Ok(Response::new(SetMaintenanceModeResponse {
                    success: false,
                    error_message: Some(e.to_string()),
                }))
            }
        }
    }

    async fn get_maintenance_status(
        &self,
        request: Request<GetMaintenanceStatusRequest>,
    ) -> Result<Response<GetMaintenanceStatusResponse>, Status> {
        let req = request.into_inner();
        info!(
            "Received GetMaintenanceStatus request, request_id: {:?}",
            req.request_id
        );
        let db = |e: mostro_core::error::MostroError| Status::internal(e.to_string());
        let counters = drain_counters(&self.pool).await.map_err(db)?;
        let reason = daemon_state::get(&self.pool, KEY_REASON)
            .await
            .map_err(db)?
            .filter(|r| !r.is_empty());
        let since = daemon_state::get(&self.pool, KEY_SINCE)
            .await
            .map_err(db)?
            .and_then(|s| s.parse::<i64>().ok());
        let stored_ln_node_pubkey = daemon_state::get(&self.pool, KEY_LN_NODE_PUBKEY)
            .await
            .map_err(db)?;
        let ln_node_pubkey = LN_STATUS
            .get()
            .map(|s| s.node_pubkey.clone())
            .unwrap_or_default();
        Ok(Response::new(GetMaintenanceStatusResponse {
            enabled: self.maintenance.is_enabled(),
            reason,
            since,
            drained: counters.drained(),
            counters: Some(DrainCounters {
                escrowed_orders: counters.escrowed_orders,
                inflight_payouts: counters.inflight_payouts,
                inflight_dev_fees: counters.inflight_dev_fees,
                open_bonds: counters.open_bonds,
                pending_bond_payouts: counters.pending_bond_payouts,
                pending_orders: counters.pending_orders,
            }),
            ln_node_pubkey,
            stored_ln_node_pubkey,
        }))
    }

    async fn validate_db_password(
        &self,
        request: Request<ValidateDbPasswordRequest>,
    ) -> Result<Response<ValidateDbPasswordResponse>, Status> {
        // Extract client address for rate limiting
        let remote_addr = request
            .remote_addr()
            .ok_or_else(|| Status::internal("Unable to determine client address"))?;

        // Check rate limit before processing
        if let Err(msg) = self
            .password_rate_limiter
            .check_rate_limit(&remote_addr)
            .await
        {
            warn!(
                "ValidateDbPassword rate-limited for client {}: {}",
                remote_addr.ip(),
                msg
            );
            return Err(Status::resource_exhausted(
                "Too many requests, try again later",
            ));
        }

        let req = request.into_inner();
        info!(
            "Received ValidateDbPassword request from {}",
            remote_addr.ip()
        );

        // Database encryption is not used. This endpoint is kept for backward
        // compatibility and always succeeds.
        let _ = req.password;
        self.password_rate_limiter
            .record_success(&remote_addr)
            .await;
        Ok(Response::new(ValidateDbPasswordResponse {
            success: true,
            error_message: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: We skip the admin service creation test that requires LND
    // since it would require a real Lightning node connection.
    // In a production environment, you would mock the LndConnector.

    #[test]
    fn test_rpc_request_response_structure() {
        // Test the structure of RPC request/response types
        let cancel_req = CancelOrderRequest {
            order_id: "test-order-id".to_string(),
            request_id: Some("test-request-id".to_string()),
            pretrade_only: None,
        };

        let cancel_resp = CancelOrderResponse {
            success: true,
            error_message: None,
        };

        assert_eq!(cancel_req.order_id, "test-order-id");
        assert!(cancel_resp.success);

        let settle_req = SettleOrderRequest {
            order_id: "test-order-id".to_string(),
            request_id: None,
        };

        let settle_resp = SettleOrderResponse {
            success: false,
            error_message: Some("Test error".to_string()),
        };

        assert_eq!(settle_req.order_id, "test-order-id");
        assert!(!settle_resp.success);
        assert_eq!(settle_resp.error_message, Some("Test error".to_string()));

        let add_solver_req = AddSolverRequest {
            solver_pubkey: "npub1...".to_string(),
            request_id: None,
        };

        let add_solver_resp = AddSolverResponse {
            success: true,
            error_message: None,
        };

        assert_eq!(add_solver_req.solver_pubkey, "npub1...");
        assert!(add_solver_resp.success);

        let take_dispute_req = TakeDisputeRequest {
            dispute_id: "dispute-123".to_string(),
            request_id: Some("req-456".to_string()),
        };

        let take_dispute_resp = TakeDisputeResponse {
            success: true,
            error_message: None,
        };

        assert_eq!(take_dispute_req.dispute_id, "dispute-123");
        assert_eq!(take_dispute_req.request_id, Some("req-456".to_string()));
        assert!(take_dispute_resp.success);
    }

    #[test]
    fn test_error_response_creation() {
        let error_resp = CancelOrderResponse {
            success: false,
            error_message: Some("Order not found".to_string()),
        };

        assert!(!error_resp.success);
        assert!(error_resp.error_message.is_some());
        assert_eq!(error_resp.error_message.unwrap(), "Order not found");
    }

    use crate::app::context::test_utils::test_settings;
    use crate::config::MOSTRO_CONFIG;
    use crate::rpc::admin::{GetVersionRequest, ValidateDbPasswordRequest};
    use nostr_sdk::prelude::Keys;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tonic::transport::server::TcpConnectInfo;
    use tonic::Request;

    fn init_test_settings() {
        let _ = MOSTRO_CONFIG.set(test_settings());
    }

    /// `fedimint_tonic_lnd::connect` is lazy (no network until the first
    /// RPC), so an offline connector against a dead localhost port lets us
    /// construct the full service without a live LND node.
    async fn offline_service() -> AdminServiceImpl {
        init_test_settings();
        let dir = std::env::temp_dir().join(format!("mostro-rpc-offline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cert = dir.join("tls.cert");
        let macaroon = dir.join("admin.macaroon");
        std::fs::write(&cert, b"").expect("write cert");
        std::fs::write(&macaroon, b"").expect("write macaroon");
        let client = fedimint_tonic_lnd::connect("https://127.0.0.1:1".to_string(), cert, macaroon)
            .await
            .expect("lazy connect must not touch the network");
        let ln_client = Arc::new(tokio::sync::Mutex::new(LndConnector { client }));

        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();

        let pool = Arc::new(pool);
        let maintenance = MaintenanceState::load(&pool).await.unwrap();
        AdminServiceImpl::new(Keys::generate(), pool, ln_client, maintenance)
    }

    fn request_with_addr<T>(inner: T, last_octet: u8) -> Request<T> {
        request_with_ip(inner, IpAddr::V4(Ipv4Addr::new(127, 0, 0, last_octet)))
    }

    fn request_with_ip<T>(inner: T, ip: IpAddr) -> Request<T> {
        let mut request = Request::new(inner);
        request.extensions_mut().insert(TcpConnectInfo {
            local_addr: None,
            remote_addr: Some(SocketAddr::new(ip, 50051)),
        });
        request
    }

    async fn insert_escrowed_order(pool: &Pool<Sqlite>, status: &str) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                                   amount, fiat_code, fiat_amount, created_at, expires_at,
                                   failed_payment, payment_attempts, hash)
               VALUES (?1, 'sell', 'ev', ?2, 0, 'lightning', 100000, 'USD', 100,
                       1700000000, 1700086400, 0, 0, ?3)"#,
        )
        .bind(id)
        .bind(status)
        .bind("cc".repeat(32))
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn set_maintenance_mode_persists_and_flips_flag() {
        let service = offline_service().await;
        assert!(!service.maintenance.is_enabled());

        let resp = service
            .set_maintenance_mode(request_with_addr(
                SetMaintenanceModeRequest {
                    enabled: true,
                    reason: Some("ln migration".into()),
                    request_id: None,
                },
                1,
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.success, "{:?}", resp.error_message);
        assert!(service.maintenance.is_enabled(), "in-memory flag flipped");
        assert!(
            MaintenanceState::load(&service.pool)
                .await
                .unwrap()
                .is_enabled(),
            "persisted in daemon_state"
        );

        let status = service
            .get_maintenance_status(Request::new(GetMaintenanceStatusRequest {
                request_id: None,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(status.enabled);
        assert_eq!(status.reason.as_deref(), Some("ln migration"));
        assert!(status.since.is_some());

        let resp = service
            .set_maintenance_mode(request_with_addr(
                SetMaintenanceModeRequest {
                    enabled: false,
                    reason: None,
                    request_id: None,
                },
                1,
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.success);
        assert!(!service.maintenance.is_enabled());
        let status = service
            .get_maintenance_status(Request::new(GetMaintenanceStatusRequest {
                request_id: None,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!status.enabled);
        assert_eq!(status.reason, None);
        assert_eq!(status.since, None);
    }

    #[tokio::test]
    async fn set_maintenance_mode_rejects_non_loopback_peer() {
        let service = offline_service().await;
        for ip in [
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
            IpAddr::V6("2001:db8::1".parse().unwrap()),
        ] {
            let err = service
                .set_maintenance_mode(request_with_ip(
                    SetMaintenanceModeRequest {
                        enabled: true,
                        reason: None,
                        request_id: None,
                    },
                    ip,
                ))
                .await
                .expect_err("non-loopback must be refused");
            assert_eq!(err.code(), tonic::Code::PermissionDenied, "{ip}");
        }
        assert!(!service.maintenance.is_enabled(), "flag untouched");
        assert!(!MaintenanceState::load(&service.pool)
            .await
            .unwrap()
            .is_enabled());

        // IPv6 loopback is accepted like 127.0.0.1.
        let resp = service
            .set_maintenance_mode(request_with_ip(
                SetMaintenanceModeRequest {
                    enabled: true,
                    reason: None,
                    request_id: None,
                },
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.success);
    }

    fn with_token(mut service: AdminServiceImpl, token: &str) -> AdminServiceImpl {
        service.auth_token = Some(SecretString::from(token.to_owned()));
        service
    }

    fn bearer<T>(mut request: Request<T>, token: &str) -> Request<T> {
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        request
    }

    fn enable_req() -> SetMaintenanceModeRequest {
        SetMaintenanceModeRequest {
            enabled: true,
            reason: None,
            request_id: None,
        }
    }

    /// A forwarder (SSH tunnel, sidecar) shows up as a loopback peer; with a
    /// token configured that is no longer enough.
    #[tokio::test]
    async fn forwarded_loopback_call_without_token_is_refused() {
        let service = with_token(offline_service().await, "s3cret");
        for request in [
            request_with_addr(enable_req(), 1),
            bearer(request_with_addr(enable_req(), 1), "wrong"),
            bearer(request_with_addr(enable_req(), 1), "s3cre"),
            bearer(request_with_addr(enable_req(), 1), "s3cret0"),
        ] {
            let err = service
                .set_maintenance_mode(request)
                .await
                .expect_err("must be refused");
            assert_eq!(err.code(), tonic::Code::PermissionDenied);
        }
        assert!(!service.maintenance.is_enabled(), "flag untouched");
        assert!(!MaintenanceState::load(&service.pool)
            .await
            .unwrap()
            .is_enabled());
    }

    #[tokio::test]
    async fn correct_bearer_token_is_accepted() {
        let service = with_token(offline_service().await, "s3cret");
        let resp = service
            .set_maintenance_mode(bearer(request_with_addr(enable_req(), 1), "s3cret"))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.success);
        assert!(service.maintenance.is_enabled());
    }

    #[tokio::test]
    async fn token_also_guards_the_other_mutating_rpcs() {
        let service = with_token(offline_service().await, "s3cret");
        let deny = tonic::Code::PermissionDenied;
        assert_eq!(
            service
                .cancel_order(Request::new(CancelOrderRequest {
                    order_id: "x".into(),
                    request_id: None,
                    pretrade_only: None,
                }))
                .await
                .unwrap_err()
                .code(),
            deny
        );
        assert_eq!(
            service
                .settle_order(Request::new(SettleOrderRequest {
                    order_id: "x".into(),
                    request_id: None,
                }))
                .await
                .unwrap_err()
                .code(),
            deny
        );
        assert_eq!(
            service
                .add_solver(Request::new(AddSolverRequest {
                    solver_pubkey: "x".into(),
                    request_id: None,
                }))
                .await
                .unwrap_err()
                .code(),
            deny
        );
        assert_eq!(
            service
                .take_dispute(Request::new(TakeDisputeRequest {
                    dispute_id: "x".into(),
                    request_id: None,
                }))
                .await
                .unwrap_err()
                .code(),
            deny
        );
        // Read-only calls stay open.
        assert!(service
            .get_maintenance_status(Request::new(GetMaintenanceStatusRequest {
                request_id: None
            }))
            .await
            .is_ok());
    }

    #[test]
    fn constant_time_eq_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"ab", b"abc"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b""), "an empty token never matches");
    }

    #[tokio::test]
    async fn set_maintenance_mode_requires_remote_addr() {
        let service = offline_service().await;
        let err = service
            .set_maintenance_mode(Request::new(SetMaintenanceModeRequest {
                enabled: true,
                reason: None,
                request_id: None,
            }))
            .await
            .expect_err("no remote_addr must be an internal error");
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(!service.maintenance.is_enabled());
    }

    #[tokio::test]
    async fn get_maintenance_status_reports_counters() {
        let service = offline_service().await;
        async fn status(s: &AdminServiceImpl) -> GetMaintenanceStatusResponse {
            s.get_maintenance_status(Request::new(GetMaintenanceStatusRequest {
                request_id: None,
            }))
            .await
            .unwrap()
            .into_inner()
        }

        let st = status(&service).await;
        assert!(st.drained, "empty database is drained");
        assert_eq!(st.counters.unwrap().escrowed_orders, 0);
        // `LN_STATUS` is a process-global OnceLock other tests may have set.
        let expected = LN_STATUS
            .get()
            .map(|s| s.node_pubkey.clone())
            .unwrap_or_default();
        assert_eq!(st.ln_node_pubkey, expected);
        assert_eq!(st.stored_ln_node_pubkey, None);

        let id = insert_escrowed_order(&service.pool, "active").await;
        let st = status(&service).await;
        assert!(!st.drained);
        assert_eq!(st.counters.unwrap().escrowed_orders, 1);

        sqlx::query("UPDATE orders SET status = 'success' WHERE id = ?1")
            .bind(id)
            .execute(service.pool.as_ref())
            .await
            .unwrap();
        let st = status(&service).await;
        assert!(st.drained, "settled order no longer binds the node");
        assert_eq!(st.counters.unwrap().escrowed_orders, 0);

        daemon_state::set(&service.pool, KEY_LN_NODE_PUBKEY, "02abc")
            .await
            .unwrap();
        let st = status(&service).await;
        assert_eq!(st.stored_ln_node_pubkey.as_deref(), Some("02abc"));
    }

    #[tokio::test]
    async fn cancel_order_with_invalid_uuid_reports_failure() {
        let service = offline_service().await;
        let response = service
            .cancel_order(Request::new(CancelOrderRequest {
                order_id: "not-a-uuid".to_string(),
                request_id: Some("7".to_string()),
                pretrade_only: None,
            }))
            .await
            .expect("RPC surface always answers with a response");
        let inner = response.into_inner();
        assert!(!inner.success);
        assert!(inner.error_message.is_some());
    }

    #[tokio::test]
    async fn cancel_order_with_unknown_order_reports_failure() {
        let service = offline_service().await;
        let response = service
            .cancel_order(Request::new(CancelOrderRequest {
                order_id: uuid::Uuid::new_v4().to_string(),
                request_id: None,
                pretrade_only: None,
            }))
            .await
            .expect("RPC surface always answers with a response");
        // Either the Nostr client is uninitialized or the order lookup
        // fails — both must surface as an unsuccessful response.
        assert!(!response.into_inner().success);
    }

    #[tokio::test]
    async fn settle_order_with_invalid_uuid_reports_failure() {
        let service = offline_service().await;
        let response = service
            .settle_order(Request::new(SettleOrderRequest {
                order_id: "definitely not a uuid".to_string(),
                request_id: None,
            }))
            .await
            .expect("RPC surface always answers with a response");
        assert!(!response.into_inner().success);
    }

    #[tokio::test]
    async fn settle_order_with_unknown_order_reports_failure() {
        let service = offline_service().await;
        let response = service
            .settle_order(Request::new(SettleOrderRequest {
                order_id: uuid::Uuid::new_v4().to_string(),
                request_id: Some("9".to_string()),
            }))
            .await
            .expect("RPC surface always answers with a response");
        assert!(!response.into_inner().success);
    }

    #[tokio::test]
    async fn take_dispute_with_invalid_uuid_reports_failure() {
        let service = offline_service().await;
        let response = service
            .take_dispute(Request::new(TakeDisputeRequest {
                dispute_id: "nope".to_string(),
                request_id: None,
            }))
            .await
            .expect("RPC surface always answers with a response");
        assert!(!response.into_inner().success);
    }

    #[tokio::test]
    async fn take_dispute_with_unknown_dispute_reports_failure() {
        let service = offline_service().await;
        let response = service
            .take_dispute(Request::new(TakeDisputeRequest {
                dispute_id: uuid::Uuid::new_v4().to_string(),
                request_id: Some("11".to_string()),
            }))
            .await
            .expect("RPC surface always answers with a response");
        assert!(!response.into_inner().success);
    }

    #[tokio::test]
    async fn add_solver_answers_without_transport_error() {
        let service = offline_service().await;
        // Depending on global Nostr-client state the action may succeed or
        // fail; the RPC surface must answer with a response either way.
        let response = service
            .add_solver(Request::new(AddSolverRequest {
                solver_pubkey: Keys::generate().public_key().to_hex(),
                request_id: Some("13".to_string()),
            }))
            .await;
        assert!(response.is_ok());
    }

    #[tokio::test]
    async fn get_version_returns_crate_version() {
        let service = offline_service().await;
        let response = service
            .get_version(Request::new(GetVersionRequest {}))
            .await
            .expect("get_version never fails");
        assert_eq!(response.into_inner().version, env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn validate_db_password_requires_remote_addr() {
        let service = offline_service().await;
        let status = service
            .validate_db_password(Request::new(ValidateDbPasswordRequest {
                password: "secret".to_string(),
            }))
            .await
            .expect_err("no remote_addr must be an internal error");
        assert_eq!(status.code(), tonic::Code::Internal);
    }

    #[tokio::test]
    async fn validate_db_password_succeeds_with_remote_addr() {
        let service = offline_service().await;
        let response = service
            .validate_db_password(request_with_addr(
                ValidateDbPasswordRequest {
                    password: "anything".to_string(),
                },
                21,
            ))
            .await
            .expect("backward-compat endpoint always succeeds");
        assert!(response.into_inner().success);
    }

    #[tokio::test]
    async fn validate_db_password_is_rate_limited_after_failures() {
        let service = offline_service().await;
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 22)), 50051);
        // Pause the tokio clock only for the failure loop so the
        // exponential-backoff sleeps auto-advance instead of stalling the
        // suite ~15s; the std-`Instant` lockout stays active in real time.
        tokio::time::pause();
        // Drive the limiter into lockout directly (5 failures).
        for _ in 0..5 {
            service.password_rate_limiter.record_failure(&addr).await;
        }
        tokio::time::resume();
        let status = service
            .validate_db_password(request_with_addr(
                ValidateDbPasswordRequest {
                    password: "anything".to_string(),
                },
                22,
            ))
            .await
            .expect_err("locked-out client must be refused");
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
    }

    #[test]
    fn test_optional_fields() {
        // Test that optional fields work correctly
        let req_with_request_id = CancelOrderRequest {
            order_id: "order1".to_string(),
            request_id: Some("req1".to_string()),
            pretrade_only: None,
        };

        let req_without_request_id = CancelOrderRequest {
            order_id: "order2".to_string(),
            request_id: None,
            pretrade_only: None,
        };

        assert!(req_with_request_id.request_id.is_some());
        assert!(req_without_request_id.request_id.is_none());
    }

    /// `pretrade_only` must never fall through to the dispute cancel: a
    /// disputed order is refused with an explanatory message and nothing
    /// is touched.
    #[tokio::test]
    async fn cancel_order_pretrade_only_refuses_a_dispute() {
        let service = offline_service().await;
        let id = insert_escrowed_order(service.pool.as_ref(), "dispute").await;
        let response = service
            .cancel_order(Request::new(CancelOrderRequest {
                order_id: id.to_string(),
                request_id: None,
                pretrade_only: Some(true),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!response.success);
        let msg = response.error_message.unwrap_or_default();
        assert!(
            msg.contains("is dispute") && msg.contains("pretrade_only"),
            "{msg}"
        );
        let status: String = sqlx::query_scalar("SELECT status FROM orders WHERE id = ?")
            .bind(id)
            .fetch_one(service.pool.as_ref())
            .await
            .unwrap();
        assert_eq!(status, "dispute");
    }

    /// The "use the dispute flow" hint is only right when the order really
    /// is in dispute; for any other non-pretrade status that flow would be
    /// refused too, so the message must not send the operator there.
    #[tokio::test]
    async fn cancel_order_pretrade_only_hints_dispute_flow_only_for_disputes() {
        let service = offline_service().await;
        let disputed = insert_escrowed_order(service.pool.as_ref(), "dispute").await;
        let active = insert_escrowed_order(service.pool.as_ref(), "active").await;
        let msg_for = |id: uuid::Uuid| {
            let service = &service;
            async move {
                service
                    .cancel_order(Request::new(CancelOrderRequest {
                        order_id: id.to_string(),
                        request_id: None,
                        pretrade_only: Some(true),
                    }))
                    .await
                    .unwrap()
                    .into_inner()
                    .error_message
                    .unwrap_or_default()
            }
        };
        let disputed_msg = msg_for(disputed).await;
        assert!(disputed_msg.contains("dispute flow"), "{disputed_msg}");
        let active_msg = msg_for(active).await;
        assert!(active_msg.contains("is active"), "{active_msg}");
        assert!(!active_msg.contains("dispute flow"), "{active_msg}");
    }

    /// A pending order passes the guard and reaches the cancel handler
    /// (which then fails on the uninitialised Nostr client in this offline
    /// test — the point is that the refusal reason is not the guard's).
    #[tokio::test]
    async fn cancel_order_pretrade_only_lets_a_pending_order_through() {
        let service = offline_service().await;
        let id = insert_escrowed_order(service.pool.as_ref(), "pending").await;
        let response = service
            .cancel_order(Request::new(CancelOrderRequest {
                order_id: id.to_string(),
                request_id: None,
                pretrade_only: Some(true),
            }))
            .await
            .unwrap()
            .into_inner();
        let msg = response.error_message.unwrap_or_default();
        assert!(!msg.contains("pretrade_only was requested"), "{msg}");
    }

    #[tokio::test]
    async fn cancel_order_pretrade_only_reports_unknown_order() {
        let service = offline_service().await;
        let response = service
            .cancel_order(Request::new(CancelOrderRequest {
                order_id: uuid::Uuid::new_v4().to_string(),
                request_id: None,
                pretrade_only: Some(true),
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!response.success);
        assert!(response
            .error_message
            .unwrap_or_default()
            .contains("not found"));
    }
}
