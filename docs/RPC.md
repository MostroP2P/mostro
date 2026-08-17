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
# Bearer token required on every admin RPC call. Required when enabled = true
# (the daemon refuses to start otherwise). Prefer the MOSTRO_RPC_AUTH_TOKEN
# environment variable over storing this in plaintext TOML.
auth_token = "change-me-to-a-long-random-value"
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

Kept for backward compatibility with older clients. The SQLite database is **not** encrypted and this RPC does **not** validate any password; it always succeeds. Like every other admin RPC, it is only reached after the bearer-token check below passes.

**Request:**

- `password`: Ignored (kept in the protobuf for compatibility only)

**Response:**

- `success`: Always `true`
- `error_message`: Always `None`

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
    
    let mut request = tonic::Request::new(CancelOrderRequest {
        order_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        request_id: Some("12345".to_string()),
    });
    request.metadata_mut().insert(
        "authorization",
        "Bearer <your auth_token>".parse().unwrap(),
    );
    
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

- Every admin RPC call requires a bearer token: `authorization: Bearer <token>` metadata, checked by `TokenAuthInterceptor` (`src/rpc/auth.rs`) before the request reaches any handler. The comparison is constant-time (`subtle::ConstantTimeEq`) so a timing side channel can't be used to guess the token.
- The token is set via `[rpc].auth_token` in `settings.toml`, or the `MOSTRO_RPC_AUTH_TOKEN` environment variable (or `<settings_dir>/.env`) — the environment variable takes precedence, following the same pattern as `MOSTRO_NSEC_PRIVKEY`.
- **Fail-closed at startup**: if `[rpc].enabled = true` and no token is configured (TOML or environment), the daemon refuses to start rather than silently running an authless admin surface.
- The RPC server listens on localhost by default for security
- The RPC interface provides the same admin capabilities as Nostr-based commands
- Only enable the RPC server in trusted environments, and treat the token like any other credential (rotate it, don't commit it, prefer the environment variable over plaintext TOML)

## Debugging

When RPC is enabled, you'll see log messages like:

```text
INFO mostro::rpc::server: Starting RPC server on 127.0.0.1:50051
INFO mostro::rpc::server: RPC server started successfully
```

Admin operations will be logged:

```text
INFO mostro::rpc::service: Received cancel order request for order: 550e8400-e29b-41d4-a716-446655440000
```

## Integration with Existing Nostr Commands

The RPC interface reuses the existing admin command handlers, ensuring consistency between RPC and Nostr-based operations:

- `AdminCancel` → `CancelOrder` RPC
- `AdminSettle` → `SettleOrder` RPC  
- `AdminAddSolver` → `AddSolver` RPC
- `AdminTakeDispute` → `TakeDispute` RPC

Both interfaces share the same business logic and database operations.
