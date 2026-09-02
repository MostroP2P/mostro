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

Cancel an order as an admin. Two cases are accepted:

- An order in `dispute`: the assigned solver's resolution. The escrow hold
  invoice is cancelled (funds return to the seller), the dispute is closed as
  `seller-refunded`, both parties are notified, and the optional
  `BondResolution` is applied. A hold invoice LND already canceled (CLTV
  expiry refunded the seller) or does not know (the daemon now runs against a
  different node, `[lightning].allow_node_change = true`) is treated as done,
  so stale disputes can still be closed; only a transient LND failure aborts
  the cancel. In the different-node case the seller's HTLC is still held by
  the old node and is refunded there when it expires, not by this call; the
  daemon logs a warning saying so.
- An order still pre-trade — `pending`, or `waiting-taker-bond` while a
  taker is mid-bond — with no escrow: an **operator** cancel.
  The row flips to `canceled-by-admin`, the maker and any bonded prospective
  taker receive `AdminCanceled`, taker bonds are released and the maker bond
  is resolved at range close (released, or settled once if a slice was
  slashed). Only the daemon key is allowed — i.e. this RPC; a solver sending
  `AdminCancel` over Nostr for a pending order is refused with
  `NotAuthorized`. Use it to drain open range-order maker bonds ahead of a
  Lightning node migration instead of waiting for `max_expiration_days`
  (see `docs/MAINTENANCE_MODE_LN_MIGRATION.md` §5). A take that commits
  concurrently wins: the cancel then fails with `NotAllowedByStatus` and the
  order continues.

Any other status is refused with `NotAllowedByStatus`.

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

### 7. Set Maintenance Mode

Enable or disable maintenance ("drain") mode. While enabled the daemon answers `new-order`, `take-buy` and `take-sell` with `cant-do` reason `maintenance_mode` and persists nothing for them; every action on an existing order, every admin action and every scheduler job keeps working so open escrow can finish on the current Lightning node. The flag is stored in the `daemon_state` table and survives restarts. See `docs/MAINTENANCE_MODE_LN_MIGRATION.md`.

**Loopback only.** This mutating call is refused with `PERMISSION_DENIED` unless the peer address is a loopback address (`127.0.0.0/8` or `::1`). Requests without peer information are refused with `INTERNAL`. A forwarder (SSH tunnel, sidecar, reverse proxy) shows up as a loopback peer, so if the port is reachable through one, configure `[rpc].auth_token` (see [Authentication](#authentication)).

**Request:**

- `enabled`: `true` to enter maintenance mode, `false` to leave it
- `reason`: Optional free text stored with the flag (shown by `GetMaintenanceStatus`, never published)
- `request_id`: Optional request identifier for tracking

**Response:**

- `success`: Boolean indicating whether the flag was persisted and applied
- `error_message`: Error details if the write failed

### 8. Get Maintenance Status

The maintenance flag plus the counters of what is still bound to the connected Lightning node. Poll it while draining: once `drained` is `true` the daemon can be stopped and pointed at a different node.

**Request:**

- `request_id`: Optional request identifier for tracking

**Response:**

- `enabled`: Current maintenance flag
- `reason`: The text given on the last enable, if any
- `since`: Unix seconds of the last enable; absent while disabled
- `counters`:
  - `escrowed_orders`: orders with a hold invoice in a non-terminal status
  - `inflight_payouts`: buyer payouts in flight (`settled-hold-invoice` with a payout hash)
  - `unpaid_dev_fees`: successful orders whose dev fee is still unpaid
  - `open_bonds`: bond hold invoices still open (`requested` / `locked`)
  - `pending_bond_payouts`: bonds waiting for, or in the middle of, their payout
  - `pending_orders`: informational — pending orders hold no escrow and do not block a switch
- `drained`: `true` when every counter except `pending_orders` is zero
- `ln_node_pubkey`: identity pubkey of the connected Lightning node (empty if unknown)
- `stored_ln_node_pubkey`: pubkey persisted by the boot node-identity guard, once that ships

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
  rpc SetMaintenanceMode(SetMaintenanceModeRequest) returns (SetMaintenanceModeResponse);
  rpc GetMaintenanceStatus(GetMaintenanceStatusRequest) returns (GetMaintenanceStatusResponse);
}
```

With [`grpcurl`](https://github.com/fullstorydev/grpcurl):

```bash
grpcurl -plaintext -import-path proto -proto admin.proto \
  -d '{"enabled": true, "reason": "LN node migration"}' \
  127.0.0.1:50051 mostro.admin.v1.AdminService/SetMaintenanceMode

grpcurl -plaintext -import-path proto -proto admin.proto \
  127.0.0.1:50051 mostro.admin.v1.AdminService/GetMaintenanceStatus

# Operator cancel of a pending order (add -H only when auth_token is set)
grpcurl -plaintext -import-path proto -proto admin.proto \
  -H 'authorization: Bearer <token>' \
  -d '{"order_id": "<uuid>"}' \
  127.0.0.1:50051 mostro.admin.v1.AdminService/CancelOrder
```

`mostro-cli admcancel` is **not** this RPC: it sends `AdminCancel` over Nostr
signed with `ADMIN_NSEC`, so it reaches the pending-order path only when that
key is the daemon's own.

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

## Authentication

Optional. Set a shared secret in `settings.toml`:

```toml
[rpc]
auth_token = "a-long-random-string"
```

When set, every mutating RPC (`CancelOrder`, `SettleOrder`, `AddSolver`, `TakeDispute`, `SetMaintenanceMode`) must carry the gRPC metadata header `authorization: Bearer <token>`; a missing or wrong token is refused with `PERMISSION_DENIED` before anything is read or written. The comparison is constant-time. Read-only calls (`GetVersion`, `GetMaintenanceStatus`, `ValidateDbPassword`) are not affected. When unset, the historical bind-address-only model applies.

```bash
grpcurl -plaintext -import-path proto -proto admin.proto \
  -H 'authorization: Bearer a-long-random-string' \
  -d '{"enabled": true}' \
  127.0.0.1:50051 mostro.admin.v1.AdminService/SetMaintenanceMode
```

## Security Considerations

- The RPC server listens on localhost by default for security
- Set `[rpc].auth_token` whenever the port is reachable through anything other than the local machine (tunnel, sidecar, non-loopback bind); the peer address alone is not authorization
- `SetMaintenanceMode` additionally refuses non-loopback peers
- The RPC interface provides the same admin capabilities as Nostr-based commands
- Only enable the RPC server in trusted environments

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
