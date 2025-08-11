// Example RPC client for Mostro version check
// Run with: cargo run --example test_version_rpc

use tonic::transport::Channel;

// Include the generated protobuf code
pub mod admin {
    tonic::include_proto!("mostro.admin.v1");
}

use admin::{admin_service_client::AdminServiceClient, GetVersionRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Connecting to Mostro RPC server...");

    // Connect to the RPC server
    let channel = Channel::from_static("http://127.0.0.1:50051")
        .connect()
        .await?;

    let mut client = AdminServiceClient::new(channel);
    println!("Connected! Calling GetVersion...");

    let request = tonic::Request::new(GetVersionRequest {});
    let response = client.get_version(request).await?;
    let version = response.get_ref().version.clone();

    println!("Mostro version: {}", version);
    Ok(())
}
