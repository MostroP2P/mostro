//! RPC service implementation for admin operations

use crate::lightning::LndConnector;
use crate::rpc::admin::{
    admin_service_server::AdminService, AddSolverRequest, AddSolverResponse, CancelOrderRequest,
    CancelOrderResponse, SettleOrderRequest, SettleOrderResponse, TakeDisputeRequest,
    TakeDisputeResponse,
};
use nostr_sdk::{nips::nip59::UnwrappedGift, Keys};
use sqlx::{Pool, Sqlite};
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{error, info};

/// Implementation of the AdminService gRPC service
pub struct AdminServiceImpl {
    keys: Keys,
    pool: Arc<Pool<Sqlite>>,
    ln_client: Arc<tokio::sync::Mutex<LndConnector>>,
}

impl AdminServiceImpl {
    pub fn new(
        keys: Keys,
        pool: Arc<Pool<Sqlite>>,
        ln_client: Arc<tokio::sync::Mutex<LndConnector>>,
    ) -> Self {
        Self {
            keys,
            pool,
            ln_client,
        }
    }

    /// Convert admin actions to use existing handlers
    /// This creates the necessary structures to call existing admin handlers
    async fn call_admin_cancel(
        &self,
        order_id: String,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::app::admin_cancel::admin_cancel_action;
        use mostro_core::message::{Action, Message};
        use nostr_sdk::{Kind as NostrKind, Timestamp, UnsignedEvent};
        use uuid::Uuid;

        // Create a mock message for the admin cancel action
        let msg = Message::new_order(
            Some(Uuid::parse_str(&order_id)?),
            request_id.map(|id| id.parse().unwrap_or(1)),
            None,
            Action::AdminCancel,
            None,
        );

        // Create a mock UnwrappedGift event
        let unsigned_event = UnsignedEvent::new(
            self.keys.public_key(),
            Timestamp::now(),
            NostrKind::GiftWrap,
            Vec::new(),
            "",
        );

        let event = UnwrappedGift {
            sender: self.keys.public_key(),
            rumor: unsigned_event,
        };

        let mut ln_client = self.ln_client.lock().await;
        admin_cancel_action(msg, &event, &self.keys, &self.pool, &mut ln_client)
            .await
            .map_err(|e| format!("Admin cancel failed: {}", e))?;

        Ok(())
    }

    async fn call_admin_settle(
        &self,
        order_id: String,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::app::admin_settle::admin_settle_action;
        use mostro_core::message::{Action, Message};
        use nostr_sdk::{Kind as NostrKind, Timestamp, UnsignedEvent};
        use uuid::Uuid;

        let msg = Message::new_order(
            Some(Uuid::parse_str(&order_id)?),
            request_id.and_then(|id| id.parse::<u64>().ok()),
            None,
            Action::AdminSettle,
            None,
        );

        let unsigned_event = UnsignedEvent::new(
            self.keys.public_key(),
            Timestamp::now(),
            NostrKind::GiftWrap,
            Vec::new(),
            "",
        );

        let event = UnwrappedGift {
            sender: self.keys.public_key(),
            rumor: unsigned_event,
        };

        let mut ln_client = self.ln_client.lock().await;
        admin_settle_action(msg, &event, &self.keys, &self.pool, &mut ln_client)
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
        use nostr_sdk::{Kind as NostrKind, Timestamp, UnsignedEvent};

        let msg = Message::new_dispute(
            None,
            request_id.and_then(|id| id.parse::<u64>().ok()),
            None,
            Action::AdminAddSolver,
            Some(Payload::TextMessage(solver_pubkey)),
        );

        let unsigned_event = UnsignedEvent::new(
            self.keys.public_key(),
            Timestamp::now(),
            NostrKind::GiftWrap,
            Vec::new(),
            "",
        );

        let event = UnwrappedGift {
            sender: self.keys.public_key(),
            rumor: unsigned_event,
        };

        admin_add_solver_action(msg, &event, &self.keys, &self.pool)
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
        use nostr_sdk::{Kind as NostrKind, Timestamp, UnsignedEvent};
        use uuid::Uuid;

        let msg = Message::new_dispute(
            Some(Uuid::parse_str(&dispute_id)?),
            request_id.and_then(|id| id.parse::<u64>().ok()),
            None,
            Action::AdminTakeDispute,
            None,
        );

        let unsigned_event = UnsignedEvent::new(
            self.keys.public_key(),
            Timestamp::now(),
            NostrKind::GiftWrap,
            Vec::new(),
            "",
        );

        let event = UnwrappedGift {
            sender: self.keys.public_key(),
            rumor: unsigned_event,
        };

        admin_take_dispute_action(msg, &event, &self.keys, &self.pool)
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
        let req = request.into_inner();
        info!("Received cancel order request for order: {}", req.order_id);

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

    #[test]
    fn test_optional_fields() {
        // Test that optional fields work correctly
        let req_with_request_id = CancelOrderRequest {
            order_id: "order1".to_string(),
            request_id: Some("req1".to_string()),
        };

        let req_without_request_id = CancelOrderRequest {
            order_id: "order2".to_string(),
            request_id: None,
        };

        assert!(req_with_request_id.request_id.is_some());
        assert!(req_without_request_id.request_id.is_none());
    }
}
