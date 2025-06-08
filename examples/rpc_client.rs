// Example RPC client for Mostro admin operations
// Run with: cargo run --example rpc_client

use tonic::transport::Channel;

// Include the generated protobuf code
pub mod admin {
    tonic::include_proto!("mostro.admin.v1");
}

use admin::{admin_service_client::AdminServiceClient, CancelOrderRequest, AddSolverRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to the RPC server
    let channel = Channel::from_static("http://127.0.0.1:50051")
        .connect()
        .await?;
    
    let mut client = AdminServiceClient::new(channel);
    
    // Example 1: Cancel an order
    println!("Attempting to cancel order...");
    let cancel_request = tonic::Request::new(CancelOrderRequest {
        order_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        request_id: Some("12345".to_string()),
    });
    
    match client.cancel_order(cancel_request).await {
        Ok(response) => {
            let resp = response.get_ref();
            if resp.success {
                println!("✅ Order cancelled successfully");
            } else {
                println!("❌ Failed to cancel order: {:?}", resp.error_message);
            }
        }
        Err(e) => {
            println!("❌ RPC Error: {}", e);
        }
    }
    
    // Example 2: Add a solver
    println!("\nAttempting to add solver...");
    let add_solver_request = tonic::Request::new(AddSolverRequest {
        solver_pubkey: "npub1example...".to_string(),
        request_id: Some("67890".to_string()),
    });
    
    match client.add_solver(add_solver_request).await {
        Ok(response) => {
            let resp = response.get_ref();
            if resp.success {
                println!("✅ Solver added successfully");
            } else {
                println!("❌ Failed to add solver: {:?}", resp.error_message);
            }
        }
        Err(e) => {
            println!("❌ RPC Error: {}", e);
        }
    }
    
    Ok(())
}