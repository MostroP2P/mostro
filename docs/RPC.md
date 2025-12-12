# Mostro RPC Interface

This document describes the RPC interface for direct admin communication with Mostro daemon.

## Overview

The RPC interface provides a direct communication method for admin operations, complementing the existing Nostr-based communication. This is particularly useful for:

- Local development and debugging
- Admin applications that need low-latency access
- Systems like Start9 or Umbrel that prefer direct communication

## Configuration

Add the following section to your `settings.toml` (keys are required; fields have Rust Default implementations but must be present):

```toml
[rpc]
# Enable RPC server for direct admin communication (required key; default=false)
enabled = true
# RPC server listen address (required key; default="127.0.0.1")
listen_address = "127.0.0.1"
# RPC server port (required key; default=50051)
port = 50051
```

## Available Admin Operations

The RPC interface supports the following admin operations:

### 1. Cancel Order
Cancel an order as an admin.

**Request:**
- `order_id`: UUID of the order to cancel
- `request_id`: Optional request identifier

**Response:**
- `success`: Boolean indicating operation success
- `error_message`: Optional error message if operation failed

### 2. Settle Order
Settle a disputed order as an admin.

**Request:**
- `order_id`: UUID of the order to settle
- `request_id`: Optional request identifier

**Response:**
- `success`: Boolean indicating operation success
- `error_message`: Optional error message if operation failed

### 3. Add Solver
Add a new dispute solver.

**Request:**
- `solver_pubkey`: Public key of the solver to add (in bech32 format)
- `request_id`: Optional request identifier

**Response:**
- `success`: Boolean indicating operation success
- `error_message`: Optional error message if operation failed

### 4. Take Dispute
Take a dispute for resolution.

**Request:**
- `dispute_id`: UUID of the dispute to take
- `request_id`: Optional request identifier

**Response:**
- `success`: Boolean indicating operation success
- `error_message`: Optional error message if operation failed

### 5. Validate Database Password
Validate the database password for encrypted databases.

**Request:**
- `password`: Database password to validate
- `request_id`: Optional request identifier

**Response:**
- `success`: Boolean indicating password validity
- `error_message`: Optional error message if validation failed

### 6. Get Version
Retrieve the Mostro daemon version.

**Request:**
- No parameters required

**Response:**
- `version`: String containing the daemon version (from CARGO_PKG_VERSION)

## Protocol Details

The RPC interface uses gRPC with Protocol Buffers. The service definition is:

```protobuf
service AdminService {
  rpc CancelOrder(CancelOrderRequest) returns (CancelOrderResponse);
  rpc SettleOrder(SettleOrderRequest) returns (SettleOrderResponse);
  rpc AddSolver(AddSolverRequest) returns (AddSolverResponse);
  rpc TakeDispute(TakeDisputeRequest) returns (TakeDisputeResponse);
  rpc ValidateDbPassword(ValidateDbPasswordRequest) returns (ValidateDbPasswordResponse);
  rpc GetVersion(GetVersionRequest) returns (GetVersionResponse);
}
```

## Client Implementation Example

Here's an example of how to create a gRPC client for the Mostro admin RPC:

```rust
use tonic::transport::Channel;
use mostro::rpc::admin::{admin_service_client::AdminServiceClient, CancelOrderRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let channel = Channel::from_static("http://127.0.0.1:50051")
        .connect()
        .await?;
    
    let mut client = AdminServiceClient::new(channel);
    
    let request = tonic::Request::new(CancelOrderRequest {
        order_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        request_id: Some("12345".to_string()),
    });
    
    let response = client.cancel_order(request).await?;
    
    if response.get_ref().success {
        println!("Order cancelled successfully");
    } else {
        println!("Failed to cancel order: {:?}", response.get_ref().error_message);
    }
    
    Ok(())
}
```

## Security Considerations

- The RPC server listens on localhost by default for security
- Consider implementing authentication/authorization for production use
- The RPC interface provides the same admin capabilities as Nostr-based commands
- Only enable the RPC server in trusted environments

## Debugging

When RPC is enabled, you'll see log messages like:

```
INFO mostro::rpc::server: Starting RPC server on 127.0.0.1:50051
INFO mostro::rpc::server: RPC server started successfully
```

Admin operations will be logged:

```
INFO mostro::rpc::service: Received cancel order request for order: 550e8400-e29b-41d4-a716-446655440000
```

## Integration with Existing Nostr Commands

The RPC interface reuses the existing admin command handlers, ensuring consistency between RPC and Nostr-based operations:

- `AdminCancel` → `CancelOrder` RPC
- `AdminSettle` → `SettleOrder` RPC  
- `AdminAddSolver` → `AddSolver` RPC
- `AdminTakeDispute` → `TakeDispute` RPC

Both interfaces share the same business logic and database operations.
