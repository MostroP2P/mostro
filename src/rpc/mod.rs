//! RPC server module for direct admin communication
//!
//! This module provides a gRPC server that allows direct communication with Mostro
//! for admin operations without going through the Nostr protocol. This is useful
//! for local development and admin applications that need low-latency access.

pub mod server;
pub mod service;

pub use server::RpcServer;

// Include the generated protobuf code
pub mod admin {
    tonic::include_proto!("mostro.admin.v1");
}

#[cfg(test)]
mod tests {
    use super::admin::*;

    #[test]
    fn test_protobuf_message_creation() {
        // Test that protobuf messages can be created correctly
        let cancel_request = CancelOrderRequest {
            order_id: "test-order".to_string(),
            request_id: Some("test-request".to_string()),
        };

        assert_eq!(cancel_request.order_id, "test-order");
        assert_eq!(cancel_request.request_id, Some("test-request".to_string()));

        let cancel_response = CancelOrderResponse {
            success: true,
            error_message: None,
        };

        assert!(cancel_response.success);
        assert!(cancel_response.error_message.is_none());
    }

    #[test]
    fn test_all_request_types() {
        // Test that all request types can be instantiated
        let _cancel_req = CancelOrderRequest {
            order_id: "order1".to_string(),
            request_id: None,
        };

        let _settle_req = SettleOrderRequest {
            order_id: "order2".to_string(),
            request_id: Some("req2".to_string()),
        };

        let _add_solver_req = AddSolverRequest {
            solver_pubkey: "npub1...".to_string(),
            request_id: None,
        };

        let _take_dispute_req = TakeDisputeRequest {
            dispute_id: "dispute1".to_string(),
            request_id: Some("req3".to_string()),
        };

        // Test passes if all types can be instantiated
        assert!(true);
    }

    #[test]
    fn test_all_response_types() {
        // Test that all response types can be instantiated
        let _cancel_resp = CancelOrderResponse {
            success: true,
            error_message: None,
        };

        let _settle_resp = SettleOrderResponse {
            success: false,
            error_message: Some("Error".to_string()),
        };

        let _add_solver_resp = AddSolverResponse {
            success: true,
            error_message: None,
        };

        let _take_dispute_resp = TakeDisputeResponse {
            success: false,
            error_message: Some("Dispute not found".to_string()),
        };

        // Test passes if all types can be instantiated
        assert!(true);
    }
}
