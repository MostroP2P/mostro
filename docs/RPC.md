# Mostro RPC Interface

This document describes the RPC interface for direct admin communication with Mostro daemon.

## Overview

The RPC interface provides a direct communication method for admin operations, complementing the existing Nostr-based communication. This is particularly useful for:

- Local development and debugging
- Admin applications that need low-latency access
- Systems like Start9 or Umbrel that prefer direct communication

## Configuration

Add the following section to your `settings.toml` (`enabled`, `listen_address` and `port` are required keys; fields have Rust Default implementations but must be present):

```toml
[rpc]
# Enable RPC server for direct admin communication (required key; default=false)
enabled = true
# RPC server listen address (required key; default="127.0.0.1")
listen_address = "127.0.0.1"
# RPC server port (required key; default=50051)
port = 50051
# Optional: acknowledge a non-loopback bind (default=false)
allow_remote = false
# Optional: serve TLS. Both paths are required together.
# tls_cert_path = "/etc/mostro/rpc-cert.pem"
# tls_key_path = "/etc/mostro/rpc-key.pem"
```

`listen_address` must be an IP literal, with IPv6 bracketed: `127.0.0.1`,
`[::1]` or `0.0.0.0`. Hostnames are never resolved, so `localhost` and
unbracketed `::1` are refused at startup rather than accepted and then failed on
at bind time: `fn validate_rpc_settings` in `src/config/util.rs` and `fn bind`
for `RpcServer` in `src/rpc/server.rs` both resolve the address through
`fn listen_socket_addr` in `src/rpc/server.rs`.

The bearer token is **not** configured here. It is read from the `MOSTRO_RPC_TOKEN`
environment variable, which the daemon also picks up from `<settings_dir>/.env`:

```bash
# ~/.mostro/.env
MOSTRO_RPC_TOKEN=<output of: openssl rand -base64 32>
```

`settings.toml` is the file operators paste into bug reports, so it never holds
the credential. The daemon **refuses to start** when `enabled = true` and the
variable is unset or shorter than 32 characters.

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

Kept for backward compatibility with older clients. The SQLite database is **not** encrypted and this RPC does **not** validate any password; it always succeeds.

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

## Authentication

Every method, `GetVersion` included, requires an `authorization: Bearer <token>`
header carrying the value of `MOSTRO_RPC_TOKEN`. The scheme is matched
case-insensitively per RFC 7235; the token itself must match exactly. A missing,
malformed or incorrect token is answered with `UNAUTHENTICATED` before the
handler runs, and the token comparison itself is constant-time
(`fn credentials_match` in `src/rpc/auth.rs`), so it does not leak how many
bytes matched. Total request latency is not claimed to be constant: header
parsing, logging and transport all contribute to it.

The token must be printable ASCII with no spaces, since it is sent verbatim as
an HTTP header. The daemon rejects anything else at startup.

```bash
grpcurl -plaintext \
  -H "authorization: Bearer $MOSTRO_RPC_TOKEN" \
  -d '{"order_id": "550e8400-e29b-41d4-a716-446655440000"}' \
  localhost:50051 mostro.admin.v1.AdminService/CancelOrder
```

> **Shared hosts:** the shell expands `$MOSTRO_RPC_TOKEN` into `grpcurl`'s
> arguments, where any local user can read it with `ps`. On a host with other
> users, drive the API from the Rust client below instead, which keeps the token
> in the process environment.

## Client Implementation Example

Here's an example of how to create a gRPC client for the Mostro admin RPC:

```rust
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;
use mostro::rpc::admin::{admin_service_client::AdminServiceClient, CancelOrderRequest};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let channel = Channel::from_static("http://127.0.0.1:50051")
        .connect()
        .await?;

    let token: MetadataValue<_> =
        format!("Bearer {}", std::env::var("MOSTRO_RPC_TOKEN")?).parse()?;

    let mut client = AdminServiceClient::with_interceptor(channel, move |mut req: Request<()>| {
        req.metadata_mut().insert("authorization", token.clone());
        Ok(req)
    });

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

Treat reaching this port as equivalent to holding the Mostro operator key.

Every RPC is executed under the daemon's own Nostr identity, and the daemon
identity is fully privileged downstream: `fn ensure_dispute_finalize_permission`
in `src/db.rs` waives its solver-category check for that key, and
`fn admin_add_solver_action` in `src/app/admin_add_solver.rs` accepts it
outright. The handlers apply no caller authorization of their own, so the bearer
token (`fn call` for `BearerAuth` in `src/rpc/auth.rs`) is the only thing between
the network and a settled dispute.

- **Never expose this port beyond loopback without TLS.** The daemon refuses to
  start on a non-loopback `listen_address` unless `allow_remote = true`, and
  warns when such a bind runs without TLS. Over plaintext, anyone on the path
  reads the bearer token and replays it.
- **The token is a credential, not a setting.** Keep it in `MOSTRO_RPC_TOKEN`
  (environment or `<settings_dir>/.env`, which the wizard writes with
  owner-only permissions), rotate it by restarting with a new value, and never
  commit it to `settings.toml`.
- **Container and appliance images publish ports easily.** Wrappers such as
  Start9 or Umbrel map container ports to the host or LAN. Verify the mapping
  before enabling the RPC; binding `0.0.0.0` inside a container whose port is
  published hands the admin API to every device on the network.
- **A compromised token is a compromised node.** An attacker who holds it can
  settle disputed orders to their own invoice or grant themselves permanent
  solver rights over Nostr, which survives a token rotation.
- The RPC interface provides the same admin capabilities as Nostr-based
  commands, without the Nostr-side key requirement.

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
