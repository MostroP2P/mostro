//! RPC service implementation for admin operations

use crate::app::context::AppContext;
use crate::app::daemon_state;
use crate::app::health::lnd_sync::LndSyncState;
use crate::app::maintenance::{
    drain_counters, MaintenanceState, KEY_LN_NODE_PUBKEY, KEY_REASON, KEY_SINCE,
};
use crate::config::settings::Settings;
use crate::config::{LN_STATUS, MESSAGE_QUEUES};

use crate::lightning::LndConnector;
use crate::rpc::admin::{
    admin_service_server::AdminService, AddSolverRequest, AddSolverResponse, CancelOrderRequest,
    CancelOrderResponse, DrainCounters, GetMaintenanceStatusRequest, GetMaintenanceStatusResponse,
    RebindReputationExportRequest, RebindReputationExportResponse, RevokeReputationImportsRequest,
    RevokeReputationImportsResponse, SetMaintenanceModeRequest, SetMaintenanceModeResponse,
    SettleOrderRequest, SettleOrderResponse, TakeDisputeRequest, TakeDisputeResponse,
    ValidateDbPasswordRequest, ValidateDbPasswordResponse,
};
use crate::rpc::rate_limiter::RateLimiter;
use mostro_core::transport::UnwrappedMessage;
use nostr_sdk::prelude::{Client, Keys};
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
    /// The process-wide LND sync state the monitor job feeds: admin
    /// settlements pay the buyer, and must defer exactly as the event loop
    /// does while LND is unsynced (#1063).
    lnd_sync: LndSyncState,
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
        lnd_sync: LndSyncState,
    ) -> Self {
        let retention_secs = Settings::get_rpc().rate_limiter_stale_duration;
        Self {
            keys,
            pool,
            ln_client,
            password_rate_limiter: Arc::new(RateLimiter::new(Duration::from_secs(retention_secs))),
            maintenance,
            lnd_sync,
            auth_token: Settings::get_rpc().auth_token.clone(),
        }
    }

    /// The context an admin action runs in: the shared maintenance flag and
    /// LND sync state, never fresh defaults.
    fn app_context(&self, nostr_client: Client, settings: Arc<Settings>) -> AppContext {
        AppContext::new(
            self.pool.clone(),
            nostr_client,
            settings,
            MESSAGE_QUEUES.queue_order_msg.clone(),
            self.keys.clone(),
        )
        .with_maintenance(self.maintenance.clone())
        .with_lnd_sync(self.lnd_sync.clone())
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

    /// Restrict local administration RPCs to loopback peers. Forwarded
    /// connections also need the configured bearer token; missing peer
    /// information is refused rather than assumed local.
    fn require_loopback<T>(request: &Request<T>, rpc: &str) -> Result<std::net::IpAddr, Status> {
        let ip = request
            .remote_addr()
            .ok_or_else(|| Status::internal("Unable to determine client address"))?
            .ip();
        if ip.is_loopback() {
            Ok(ip)
        } else {
            warn!("{rpc} refused for non-loopback peer {ip}");
            Err(Status::permission_denied(format!(
                "{rpc} is only accepted from loopback peers"
            )))
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
        let ctx = self.app_context(nostr_client, settings);
        let mut ln_client = self.ln_client.lock().await;
        admin_cancel_action(&ctx, msg, &event, &self.keys, &mut ln_client)
            .await
            .map_err(|e| format!("Admin cancel failed: {}", e))?;

        Ok(())
    }

    /// `CancelOrderRequest.pretrade_only`: refuse anything that is not
    /// still `pending` / `waiting-taker-bond` / `waiting-maker-bond`, so
    /// operator tooling that
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
            || order.check_status(OrderStatus::WaitingMakerBond).is_ok()
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
        let ctx = self.app_context(nostr_client, settings);
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
        let ctx = self.app_context(nostr_client, settings);
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
        let ctx = self.app_context(nostr_client, settings);
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
        let ip = Self::require_loopback(&request, "SetMaintenanceMode")?;
        self.require_auth(&request, "SetMaintenanceMode")?;
        let req = request.into_inner();
        info!(
            "Received SetMaintenanceMode(enabled={}) from {ip}, request_id: {:?}",
            req.enabled, req.request_id
        );
        // `lnd_unsynced` marks maintenance the daemon turned on by itself
        // (#1063); an operator using it would make readers of the health and
        // info events take their maintenance for the daemon's.
        if req.reason.as_deref().map(str::trim)
            == Some(crate::app::maintenance::REASON_LND_UNSYNCED)
        {
            warn!("SetMaintenanceMode refused: the reason is reserved for the daemon");
            return Ok(Response::new(SetMaintenanceModeResponse {
                success: false,
                error_message: Some(format!(
                    "the reason \"{}\" is reserved for maintenance the daemon enables by itself",
                    crate::app::maintenance::REASON_LND_UNSYNCED
                )),
            }));
        }
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

    async fn revoke_reputation_imports(
        &self,
        request: Request<RevokeReputationImportsRequest>,
    ) -> Result<Response<RevokeReputationImportsResponse>, Status> {
        let ip = Self::require_loopback(&request, "RevokeReputationImports")?;
        self.require_auth(&request, "RevokeReputationImports")?;
        let req = request.into_inner();
        info!(
            "Received RevokeReputationImports(key={}, imported_after={:?}) from {ip}, request_id: {:?}",
            req.issuer_key, req.imported_after, req.request_id
        );
        let failure = |message: String| {
            Ok(Response::new(RevokeReputationImportsResponse {
                success: false,
                error_message: Some(message),
                revoked: 0,
            }))
        };
        let Ok(key) = nostr_sdk::prelude::PublicKey::parse(req.issuer_key.trim()) else {
            return failure(format!("invalid issuer key: {}", req.issuer_key));
        };
        if Settings::get_reputation_import().is_some_and(|import| import.issuer_for(&key).is_some())
        {
            warn!(
                "RevokeReputationImports: issuer key {key} is still trusted; remove it from \
                 [reputation_import] and restart, or new imports with it remain possible"
            );
        }
        match crate::app::reputation::revoke::revoke_imports(
            &self.pool,
            &key,
            req.imported_after,
            &self.keys,
        )
        .await
        {
            Ok(rows) => {
                warn!(
                    "Revoked {} reputation imports signed by {} (reason: {})",
                    rows.len(),
                    key.to_hex(),
                    req.reason.as_deref().unwrap_or("none given")
                );
                Ok(Response::new(RevokeReputationImportsResponse {
                    success: true,
                    error_message: None,
                    revoked: rows.len() as u32,
                }))
            }
            Err(e) => {
                error!("RevokeReputationImports failed: {e}");
                failure(e.to_string())
            }
        }
    }

    async fn rebind_reputation_export(
        &self,
        request: Request<RebindReputationExportRequest>,
    ) -> Result<Response<RebindReputationExportResponse>, Status> {
        let ip = Self::require_loopback(&request, "RebindReputationExport")?;
        self.require_auth(&request, "RebindReputationExport")?;
        let req = request.into_inner();
        info!(
            "Received RebindReputationExport from {ip}, request_id: {:?}",
            req.request_id
        );
        let failure = |message: String| {
            Ok(Response::new(RebindReputationExportResponse {
                success: false,
                error_message: Some(message),
                previous_identity: None,
            }))
        };
        let parse = |key: &str| nostr_sdk::prelude::PublicKey::parse(key.trim()).ok();
        let (Some(identity), Some(new_identity)) = (parse(&req.identity), parse(&req.new_identity))
        else {
            return failure("identity and new_identity must be npub or hex keys".to_string());
        };
        if req.reason.trim().is_empty() {
            return failure("a reason is required".to_string());
        }
        let user = match crate::db::find_user_by_pubkey(&self.pool, identity.to_hex()).await {
            Ok(Some(user)) => user,
            Ok(None) => return failure(format!("no account for {}", identity.to_hex())),
            Err(e) => {
                error!("RebindReputationExport failed: {e}");
                return failure(e.to_string());
            }
        };
        let previous = user.reputation_exported_to.clone();
        let today =
            mostro_core::user::day_truncate(nostr_sdk::prelude::Timestamp::now().as_secs() as i64);
        match crate::db::move_reputation_binding(
            self.pool.as_ref(),
            &user.pubkey,
            previous.as_deref(),
            &new_identity.to_hex(),
            today as i64,
        )
        .await
        {
            Ok(true) => {
                warn!(
                    "reputation: admin rebound the export of {} from {} to {}: {}",
                    user.pubkey,
                    previous.as_deref().unwrap_or("none"),
                    new_identity.to_hex(),
                    req.reason.trim()
                );
                Ok(Response::new(RebindReputationExportResponse {
                    success: true,
                    error_message: None,
                    previous_identity: previous,
                }))
            }
            Ok(false) => failure("the binding changed meanwhile; try again".to_string()),
            Err(e) => {
                error!("RebindReputationExport failed: {e}");
                failure(e.to_string())
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
        AdminServiceImpl::new(
            Keys::generate(),
            pool,
            ln_client,
            maintenance,
            LndSyncState::new(),
        )
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

    /// Codex review on #1065: admin actions (SettleOrder pays the buyer)
    /// must run with the process-wide LND sync state and maintenance flag,
    /// not fresh defaults that never defer.
    #[tokio::test]
    async fn admin_contexts_share_the_monitored_state() {
        use crate::app::health::lnd_sync::LndObservation;
        let mut service = offline_service().await;
        let lnd_sync = LndSyncState::new();
        let now = chrono::Utc::now().timestamp();
        lnd_sync.observe_at_startup(
            LndObservation {
                synced_to_chain: false,
                block_height: 1,
                best_header_timestamp: now,
                network: None,
                peers: 0,
                channels_active: 0,
                channels_inactive: 0,
                channels_pending: 0,
            },
            now,
        );
        service.lnd_sync = lnd_sync;
        service
            .maintenance
            .set(&service.pool, true, Some("upgrade"))
            .await
            .unwrap();

        let ctx = service.app_context(
            Client::default(),
            Arc::new(crate::app::context::test_utils::test_settings()),
        );

        assert!(ctx.lnd_sync().defers_outgoing_payments(now));
        assert!(ctx.maintenance().is_enabled());
    }

    #[tokio::test]
    async fn set_maintenance_mode_rejects_the_reserved_reason() {
        let service = offline_service().await;

        let resp = service
            .set_maintenance_mode(request_with_addr(
                SetMaintenanceModeRequest {
                    enabled: true,
                    reason: Some(" lnd_unsynced ".into()),
                    request_id: None,
                },
                1,
            ))
            .await
            .unwrap()
            .into_inner();

        assert!(!resp.success);
        assert!(resp.error_message.unwrap().contains("reserved"));
        assert!(!service.maintenance.is_enabled(), "nothing changed");
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
        // RevokeReputationImports also requires a loopback peer, so it gets
        // its own test (revoke_reputation_imports_needs_loopback_and_the_token).
        // Read-only calls stay open.
        assert!(service
            .get_maintenance_status(Request::new(GetMaintenanceStatusRequest {
                request_id: None
            }))
            .await
            .is_ok());
    }

    fn revoke_req(issuer_key: &str) -> RevokeReputationImportsRequest {
        RevokeReputationImportsRequest {
            issuer_key: issuer_key.to_string(),
            imported_after: None,
            reason: Some("key leaked".to_string()),
            request_id: None,
        }
    }

    #[tokio::test]
    async fn revoke_reputation_imports_refuses_non_loopback_even_with_the_token() {
        let service = with_token(offline_service().await, "s3cret");
        let key = Keys::generate().public_key().to_hex();
        for ip in ["203.0.113.1", "2001:db8::1"] {
            let err = service
                .revoke_reputation_imports(bearer(
                    request_with_ip(revoke_req(&key), ip.parse().unwrap()),
                    "s3cret",
                ))
                .await
                .unwrap_err();
            assert_eq!(err.code(), tonic::Code::PermissionDenied);
            assert!(err.message().contains("RevokeReputationImports"));
        }
    }

    #[tokio::test]
    async fn revoke_reputation_imports_preserves_the_cutoff_and_node_signature() {
        use crate::db::{add_new_user, insert_reputation_import, ReputationImportRow};
        use mostro_core::prelude::User;
        let service = with_token(offline_service().await, "s3cret");
        let key = Keys::generate().public_key();
        let other_key = Keys::generate().public_key();
        let mut cases = Vec::new();
        for (issuer, imported_at, revoked) in [
            (key, 199, false),
            (key, 200, true),
            (key, 201, true),
            (other_key, 201, false),
        ] {
            let identity = Keys::generate().public_key().to_hex();
            let trade = Keys::generate().public_key().to_hex();
            let row = ReputationImportRow {
                attestation_id: Keys::generate().public_key().to_hex(),
                issuer: "fixture".to_string(),
                issuer_key: issuer.to_hex(),
                subject: identity.clone(),
                identity_pubkey: identity.clone(),
                trade_pubkey: Some(trade.clone()),
                reviews: 5,
                rating_hundredths: 450,
                since: 1_696_204_800,
                imported_at,
            };
            let mut user = User::new(identity.clone(), 0, 0, 0, 0, 1);
            add_new_user(service.pool.as_ref(), user.clone())
                .await
                .unwrap();
            user.apply_reputation_import(&row.figures().unwrap());
            crate::db::update_user_reputation(service.pool.as_ref(), &user)
                .await
                .unwrap();
            insert_reputation_import(service.pool.as_ref(), &row)
                .await
                .unwrap();
            cases.push((identity, trade, revoked));
        }
        let mut req = revoke_req(&key.to_hex());
        req.imported_after = Some(200);
        let response = service
            .revoke_reputation_imports(bearer(request_with_addr(req, 1), "s3cret"))
            .await
            .unwrap()
            .into_inner();
        assert!(response.success, "{:?}", response.error_message);
        assert_eq!(response.revoked, 2);
        for (identity, trade, revoked) in cases {
            let user = crate::db::is_user_present(service.pool.as_ref(), identity.clone())
                .await
                .unwrap();
            assert_eq!(user.total_reviews, if revoked { 0 } else { 5 });
            assert_eq!(
                crate::db::reputation_imports_for_identity(service.pool.as_ref(), &identity)
                    .await
                    .unwrap()
                    .is_empty(),
                revoked
            );
            let events: Vec<_> = crate::config::MESSAGE_QUEUES
                .queue_order_rate
                .read()
                .await
                .iter()
                .filter(|ev| {
                    ev.tags
                        .iter()
                        .any(|tag| tag.as_slice() == ["d", trade.as_str()])
                })
                .cloned()
                .collect();
            assert_eq!(events.len(), usize::from(revoked));
            if revoked {
                events[0].verify().unwrap();
                assert_eq!(events[0].pubkey, service.keys.public_key());
                assert_eq!(
                    events[0].kind.as_u16(),
                    mostro_core::prelude::NOSTR_RATING_EVENT_KIND
                );
                assert!(events[0]
                    .tags
                    .iter()
                    .any(|tag| tag.as_slice() == ["total_reviews", "0"]));
            }
        }
    }

    #[tokio::test]
    async fn revoke_reputation_imports_needs_loopback_and_the_token() {
        let service = with_token(offline_service().await, "s3cret");
        let key = Keys::generate().public_key().to_hex();
        let no_addr = service
            .revoke_reputation_imports(Request::new(revoke_req(&key)))
            .await
            .unwrap_err();
        assert_eq!(no_addr.code(), tonic::Code::Internal);
        let no_token = service
            .revoke_reputation_imports(request_with_addr(revoke_req(&key), 1))
            .await
            .unwrap_err();
        assert_eq!(no_token.code(), tonic::Code::PermissionDenied);
        let ok = service
            .revoke_reputation_imports(bearer(request_with_addr(revoke_req(&key), 1), "s3cret"))
            .await
            .unwrap()
            .into_inner();
        assert!(ok.success);
        assert_eq!(ok.revoked, 0);
    }

    #[tokio::test]
    async fn revoke_reputation_imports_refuses_an_invalid_key_and_reverses_the_imports() {
        use crate::db::{insert_reputation_import, ReputationImportRow};
        use nostr_sdk::prelude::ToBech32;
        let service = offline_service().await;
        let bad = service
            .revoke_reputation_imports(request_with_addr(revoke_req("npub1nope"), 1))
            .await
            .unwrap()
            .into_inner();
        assert!(!bad.success);
        assert!(bad.error_message.unwrap().contains("invalid issuer key"));

        let key = Keys::generate().public_key();
        let identity = Keys::generate().public_key().to_hex();
        sqlx::query(
            "INSERT INTO users (pubkey, total_reviews, total_rating, created_at, native_created_at, \
             seeded_reviews, seeded_rating_sum, min_rating, max_rating, last_rating) \
             VALUES (?1, 5, 4.5, 1696204800, 1790000000, 5, 22.5, 5, 5, 5)",
        )
        .bind(&identity)
        .execute(service.pool.as_ref())
        .await
        .unwrap();
        insert_reputation_import(
            service.pool.as_ref(),
            &ReputationImportRow {
                attestation_id: "a".repeat(64),
                issuer: "lnp2pbot".to_string(),
                issuer_key: key.to_hex(),
                subject: "acct-1".to_string(),
                identity_pubkey: identity.clone(),
                trade_pubkey: None,
                reviews: 5,
                rating_hundredths: 450,
                since: 1_696_204_800,
                imported_at: 1_790_000_000,
            },
        )
        .await
        .unwrap();
        let done = service
            .revoke_reputation_imports(request_with_addr(revoke_req(&key.to_bech32().unwrap()), 1))
            .await
            .unwrap()
            .into_inner();
        assert!(done.success);
        assert_eq!(done.revoked, 1);
        let user = crate::db::is_user_present(service.pool.as_ref(), identity)
            .await
            .unwrap();
        assert_eq!((user.total_reviews, user.seeded_reviews), (0, 0));
        assert_eq!(user.created_at, 1_790_000_000);
        assert_eq!(
            (user.min_rating, user.max_rating, user.last_rating),
            (0, 0, 0)
        );
    }

    fn rebind_req(
        identity: &str,
        new_identity: &str,
        reason: &str,
    ) -> RebindReputationExportRequest {
        RebindReputationExportRequest {
            identity: identity.to_string(),
            new_identity: new_identity.to_string(),
            reason: reason.to_string(),
            request_id: None,
        }
    }

    #[tokio::test]
    async fn rebind_reputation_export_needs_loopback_the_token_and_a_reason() {
        let service = with_token(offline_service().await, "s3cret");
        let (a, b) = (
            Keys::generate().public_key().to_hex(),
            Keys::generate().public_key().to_hex(),
        );
        assert_eq!(
            service
                .rebind_reputation_export(Request::new(rebind_req(&a, &b, "lost phone")))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Internal
        );
        assert_eq!(
            service
                .rebind_reputation_export(request_with_addr(rebind_req(&a, &b, "lost phone"), 1))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
        let no_reason = service
            .rebind_reputation_export(bearer(
                request_with_addr(rebind_req(&a, &b, " "), 1),
                "s3cret",
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(!no_reason.success);
        assert!(no_reason.error_message.unwrap().contains("reason"));
    }

    #[tokio::test]
    async fn rebind_reputation_export_moves_the_binding_and_reports_the_old_one() {
        let service = offline_service().await;
        let identity = Keys::generate().public_key().to_hex();
        let (old, new) = (Keys::generate().public_key(), Keys::generate().public_key());
        sqlx::query(
            "INSERT INTO users (pubkey, created_at, reputation_exported_to) VALUES (?1, 1700000000, ?2)",
        )
        .bind(&identity)
        .bind(old.to_hex())
        .execute(service.pool.as_ref())
        .await
        .unwrap();
        let done = service
            .rebind_reputation_export(request_with_addr(
                rebind_req(&identity, &new.to_hex(), "support ticket 7"),
                1,
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(done.success);
        assert_eq!(done.previous_identity, Some(old.to_hex()));
        let user = crate::db::is_user_present(service.pool.as_ref(), identity)
            .await
            .unwrap();
        assert_eq!(user.reputation_exported_to, Some(new.to_hex()));

        let unknown = service
            .rebind_reputation_export(request_with_addr(
                rebind_req(&Keys::generate().public_key().to_hex(), &new.to_hex(), "x"),
                1,
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(!unknown.success);
    }

    /// An unreadable database is not a missing account: the operator must
    /// not be told there is no account when the lookup itself failed.
    #[tokio::test]
    async fn rebind_reputation_export_reports_a_database_failure_as_such() {
        let service = offline_service().await;
        let (identity, new) = (
            Keys::generate().public_key().to_hex(),
            Keys::generate().public_key().to_hex(),
        );
        sqlx::query("ALTER TABLE users RENAME TO users_gone")
            .execute(service.pool.as_ref())
            .await
            .unwrap();
        let reply = service
            .rebind_reputation_export(request_with_addr(rebind_req(&identity, &new, "x"), 1))
            .await
            .unwrap()
            .into_inner();
        assert!(!reply.success);
        let message = reply.error_message.unwrap();
        assert!(!message.contains("no account"), "{message}");
    }

    #[tokio::test]
    async fn rebind_reputation_export_refuses_non_loopback_even_with_the_token() {
        let service = with_token(offline_service().await, "s3cret");
        let (a, b) = (
            Keys::generate().public_key().to_hex(),
            Keys::generate().public_key().to_hex(),
        );
        for ip in ["203.0.113.1", "2001:db8::1"] {
            let err = service
                .rebind_reputation_export(bearer(
                    request_with_ip(rebind_req(&a, &b, "lost phone"), ip.parse().unwrap()),
                    "s3cret",
                ))
                .await
                .unwrap_err();
            assert_eq!(err.code(), tonic::Code::PermissionDenied);
            assert!(err.message().contains("RebindReputationExport"));
        }
    }

    /// The move starts from the current value, which may be none: an
    /// account that was never bound gets bound, with the day of the move.
    #[tokio::test]
    async fn rebind_reputation_export_binds_an_account_that_has_no_binding() {
        let service = offline_service().await;
        let identity = Keys::generate().public_key().to_hex();
        let new = Keys::generate().public_key().to_hex();
        sqlx::query("INSERT INTO users (pubkey, created_at) VALUES (?1, 1700000000)")
            .bind(&identity)
            .execute(service.pool.as_ref())
            .await
            .unwrap();
        let today = || {
            mostro_core::user::day_truncate(nostr_sdk::prelude::Timestamp::now().as_secs() as i64)
                as i64
        };
        let first_day = today();
        let done = service
            .rebind_reputation_export(request_with_addr(
                rebind_req(&identity, &new, "first binding by hand"),
                1,
            ))
            .await
            .unwrap()
            .into_inner();
        let last_day = today();
        assert!(done.success, "{:?}", done.error_message);
        assert_eq!(done.previous_identity, None);
        let user = crate::db::is_user_present(service.pool.as_ref(), identity)
            .await
            .unwrap();
        assert_eq!(user.reputation_exported_to, Some(new));
        let day = user.reputation_exported_at.unwrap();
        assert!(day == first_day || day == last_day, "{day}");
    }

    /// §5.2: every admin rebind is logged with the old and new identity and
    /// the reason.
    #[tokio::test]
    async fn rebind_reputation_export_logs_the_old_binding_the_new_one_and_the_reason() {
        let service = offline_service().await;
        let identity = Keys::generate().public_key().to_hex();
        let (old, new) = (
            Keys::generate().public_key().to_hex(),
            Keys::generate().public_key().to_hex(),
        );
        sqlx::query(
            "INSERT INTO users (pubkey, created_at, reputation_exported_to) VALUES (?1, 1700000000, ?2)",
        )
        .bind(&identity)
        .bind(&old)
        .execute(service.pool.as_ref())
        .await
        .unwrap();
        let (reply, logs) = crate::app::context::test_utils::logged_on_this_thread(
            service.rebind_reputation_export(request_with_addr(
                rebind_req(&identity, &new, "support ticket 7"),
                1,
            )),
        )
        .await;
        assert!(reply.unwrap().into_inner().success);
        let line = logs
            .lines()
            .find(|line| line.contains("admin rebound"))
            .unwrap_or_else(|| panic!("no audit line in {logs}"));
        for expected in [identity.as_str(), &old, &new, "support ticket 7"] {
            assert!(line.contains(expected), "{expected} missing from {line}");
        }
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
