# Lightning Operations

Core interactions with LND via `fedimint_tonic_lnd`.

## Connector
- Source: `src/lightning/mod.rs`
- Type: `LndConnector { client: Client }`
- Construct: `LndConnector::new()` using host, cert, macaroon from settings.

## Hold Invoices
- Create: `create_hold_invoice(description, amount)` → `(AddHoldInvoiceResp, preimage, hash)`.
- Subscribe: `subscribe_invoice(r_hash, sender)` streams `InvoiceState` updates.
- Settle: `settle_hold_invoice(preimage)`.
- Cancel: `cancel_hold_invoice(hash)`.

## Outgoing Payments
- `send_payment(invoice, amount, sender)`
  - Validates invoice amount or supplies `amt`.
  - Caps max fee: 1% for amounts ≤1000 sats; `Settings::get_mostro().max_routing_fee` for larger amounts.
  - Streams router updates to caller.

## Node Status
- `get_node_info()`; mapped to `LnStatus` and stored in `config::LN_STATUS`.

## Invoice Validation

Source: `src/lightning/invoice.rs`

The invoice validation module provides comprehensive validation for Lightning invoices, Lightning Addresses, and LNURL-pay requests.

### Functions

#### decode_invoice
```rust
pub fn decode_invoice(payment_request: &str) -> Result<Bolt11Invoice, MostroError>
```
Decodes a BOLT11 invoice string into a structured invoice object.

**Parameters**:
- `payment_request`: BOLT11 invoice string

**Returns**: `Bolt11Invoice` or `MostroError` (`InvoiceInvalidError` / related)

**Entry**: `src/lightning/invoice.rs` (`fn decode_invoice`)

#### is_valid_invoice
```rust
pub async fn is_valid_invoice(
    payment_request: String,
    amount: Option<u64>,
    fee: Option<u64>,
) -> Result<(), MostroError>
```
Comprehensive validation supporting:
- BOLT11 Lightning invoices
- Lightning Addresses (user@domain.com)
- LNURL-pay requests

**Validation checks**:
1. Invoice format and decoding
2. Amount matching (with fee deduction): `expected_amount = amount - fee`
3. Minimum payment amount enforcement (`mostro_settings.min_payment_amount`)
4. Expiration validation (`invoice.is_expired()`)
5. Expiration window compliance: `expires_at > now + invoice_expiration_window`
6. Lightning Address / LNURL existence via `ln_exists` (see LNURL host policy below)
7. LNURL-pay metadata must advertise `tag: payRequest`

**Entry**: `src/lightning/invoice.rs` (`fn is_valid_invoice`)

**Error cases**:
- `InvalidInvoice` / `InvoiceInvalidError`: Decoding fails or format invalid
- `WrongAmountError`: Invoice amount doesn't match expected (after fee deduction)
- `MinAmountError`: Amount below minimum threshold
- `InvoiceExpiredError`: Invoice already expired
- `ExpirationWindowTooShort`: Expires before required window

## LNURL / Lightning Address fetches

Source: `src/lnurl.rs`

Buyer- and operator-supplied Lightning Addresses and LNURL-pay strings are resolved over HTTP. Those URLs are attacker-influenced, so every GET goes through `lnurl_get`:

| Control | Behavior |
|---------|----------|
| Scheme | `http` / `https` only; userinfo rejected |
| Host policy | Rejects link-local, CGNAT (`100.64.0.0/10`), NAT64 (`64:ff9b::/96`), multicast, unspecified, documentation ranges; loopback/RFC1918 forbidden in production (test-only allow guard for local mocks) |
| DNS | Lookup capped (~2s); result pinned via reqwest `.resolve` (no rebinding) |
| Redirects | Disabled (`Policy::none()`) |
| Timeouts | Connect ~2s, full request ~4s (bounds serial message-loop stall) |
| Errors | LNURL `status: ERROR`, missing `payRequest`, amount out of range, empty `pr` → `Err` (not soft empty success) |

**Public helpers**:
- `ln_exists` — metadata probe (`payRequest` tag)
- `resolv_ln_address` — resolve to BOLT11 `pr` (LUD-12 comment when allowed)
- `extract_lnurl` — parse address/LNURL to a single `Url` (no second parse at callers)

**Payout path** (`src/app/release.rs`, `fn do_payment`):
- Resolves Lightning Addresses under the policy above
- Decodes non-empty `pr` as BOLT11 before LND `send_payment`
- Resolve/decode/`send_payment` failures go through `check_failure_retries` bookkeeping and return `Err` (does not spawn an empty status watcher or report success)

## Payment Retry System

Source: `src/scheduler.rs` (`fn job_retry_failed_payments`)

Failed outgoing payments are automatically retried via the scheduler.

### Configuration

**Settings** (`src/config/types.rs`, `LightningSettings`):
```rust
pub struct LightningSettings {
    // ... other fields ...
    pub payment_attempts: u32,        // Max retry attempts (default: 3)
    pub payment_retries_interval: u32, // Seconds between retries (default: 60)
}
```

### Retry Job

**Function**: `job_retry_failed_payments()`
- Queries database for failed payments with retry attempts remaining
- Respects `payment_attempts` limit
- Scheduled at `payment_retries_interval` frequency
- Automatically invokes `do_payment()` for each retry
- Updates payment status and attempt count in database

### Workflow
1. Payment fails initially → marked as failed in DB
2. Scheduler runs retry job every N seconds (payment_retries_interval)
3. Job finds failed payments with `attempts < payment_attempts`
4. Invokes `do_payment()` again
5. Increments attempt counter
6. Continues until success or max attempts reached

## Payment Error Handling

**Pre-flight Checks** (`LndConnector::send_payment` in `src/lightning/mod.rs`):
Before attempting payment, `send_payment()` uses `track_payment_v2` to detect duplicate attempts:

```rust
// Check if payment was previously attempted
match ln_client.router().track_payment_v2(track_req).await {
    Ok(_) => {
        error!("Aborting paying invoice with hash {hash} to buyer");
        return Err(MostroError::TrackError);
    }
    Err(_) => {
        // Payment not found, safe to proceed
    }
}
```

**Amount Validation**:
```rust
if let Some(amt_msat) = invoice.amount_milli_satoshis() {
    let invoice_amount_sats = amt_msat / 1000;
    if invoice_amount_sats != amount as u64 * 1000 {
        error!("Aborting paying invoice with wrong amount to buyer");
        return Err(MostroError::WrongAmountError);
    }
}
```

**Zero-Amount Invoice Handling**:
If invoice has no amount, the `amt` field is populated in SendPaymentRequest:
```rust
if invoice.amount_milli_satoshis().is_none() {
    req.amt = amount;
}
```

**Fee Limit Enforcement**:
```rust
let max_fee = match amount.cmp(&1000) {
    // For small amounts, use 1% but ensure minimum of 10 sats
    Ordering::Less | Ordering::Equal => (amount as f64 * 0.01).max(10.0),
    Ordering::Greater => amount as f64 * mostro_settings.max_routing_fee,
};
req.fee_limit_sat = max_fee as i64;
```

**Timeout**: 60 seconds (`SendPaymentRequest::timeout_seconds`)

## Node Information

### get_node_info
```rust
pub async fn get_node_info(&mut self) -> Result<GetInfoResponse, MostroError>
```

Retrieves LND node information including:
- Node version
- Public key
- Node alias
- Active chains (e.g., bitcoin mainnet/testnet)
- Network information
- Block height sync status

**Entry**: `src/lightning/mod.rs:260`

**Usage**: Called during startup to populate `config::LN_STATUS` (src/main.rs:86)

## Anti-Abuse Bond Operations

The optional anti-abuse bond (`[anti_abuse_bond]`, off by default) puts a
**second** hold invoice on a trade, owned by the maker and/or taker. It is
released on normal completion and on cancels before a waiting-state
timeout; it is slashed only on an explicit solver `BondResolution`
directive or a waiting-state timeout (when `slash_on_waiting_timeout =
true`). Full design: `docs/ANTI_ABUSE_BOND.md`. This section is the
operator runbook.

### Where the state lives

Every bond is one row in the `bonds` table (`src/app/bond/db.rs`,
`model.rs`). Inspect it directly:

```sql
SELECT id, order_id, role, state, amount_sats,
       parent_bond_id, child_order_id, slashed_share_sats,
       node_share_sats, slashed_reason,
       payout_attempts, invoice_request_attempts, slashed_at
  FROM bonds ORDER BY created_at;
```

`state` (string-backed, `src/app/bond/types.rs`) walks:

```text
requested → locked ─┬→ released                       (happy / cancel before timeout)
                    └→ pending-payout ─┬→ slashed      (counterparty paid their share)
                                       ├→ forfeited    (counterparty never claimed in window)
                                       └→ failed       (send_payment exhausted)
```

- **`pending-payout`** — a slash already fired. The bond HTLC was
  **settled** (claimed into Mostro's wallet) at slash time; the scheduler
  is now driving the counterparty payout. The split is frozen here:
  `node_share_sats` is the node's retained share, `amount_sats -
  node_share_sats` is owed to the winning counterparty.
- **`slashed`** — terminal success; the counterparty share was paid.
- **`forfeited`** — designed-in long-stop: the counterparty never sent a
  payout invoice within `payout_claim_window_days`. The node keeps
  `amount_sats` in full. **No operator action needed.**
- **`failed`** — `send_payment` exhausted `payout_max_retries` against a
  delivered invoice. **User-recoverable** while inside the claim window: a
  fresh `Action::AddBondInvoice` from the recipient flips the row back to
  `pending-payout`. Only past the window does it need operator attention
  (see below).
- `slashed_reason` is `lost-dispute` (solver directive) or `timeout`
  (waiting-state timeout). A cancel before the timeout is never a slash.

For range-order maker bonds the parent row stays `locked` while child
rows (`parent_bond_id` set, `child_order_id` = the taken slice) carry the
proportional per-slice slashes; the single settle happens at range close.

### Scheduler jobs

Run from `src/scheduler.rs` (see `run_jobs`):

- `job_process_bond_payouts` — drives every `pending-payout` row: requests
  a payout bolt11 from the winner (`Action::AddBondInvoice`, cadenced by
  `payout_invoice_window_seconds`), runs `send_payment`, retries up to
  `payout_max_retries`, and reconciles against LND on entry so a daemon
  restart never double-pays.
- `job_reconcile_stranded_maker_bonds` — settles and distributes a range
  maker bond at range close (per-slice counterparty shares + maker
  refund); the 5-minute sweep is the backstop if the inline close failed.

### Reading what happened in the logs

Bond transitions log through `tracing` (`bond payout: …` lines in
`src/app/bond/payout.rs`, plus slash/release lines in `flow.rs` /
`slash.rs`). To follow a solver decision, look for the `BondResolution`
on the inbound `admin-settle` / `admin-cancel` message — its wire shape is:

```json
{ "order": { "version": 1, "id": "<order-id>", "action": "admin-cancel",
  "payload": { "bond_resolution": { "slash_seller": true, "slash_buyer": false } } } }
```

`slash_seller` / `slash_buyer` are resolved to a maker- or taker-bond row
by the order kind (sell → maker is seller; buy → maker is buyer). A
`payload: null` (or absent) means **release both bonds** — no slash. A
slash directed at a side with no `locked` bond is rejected with
`CantDo(InvalidPayload)` and the trade resolution does not run.

### Resolving a `failed` bond manually

A `failed` row means the bond was slashed (sats are already in Mostro's
wallet), but Mostro could not route the counterparty's share and the
claim window has since elapsed, so the auto-recovery path no longer
re-arms it. There is no slash to undo and no funds at risk on the
counterparty's side — the value is held by the node. To make the
counterparty whole, pay them out-of-band (the amount owed is
`amount_sats - node_share_sats`) and keep the row as the audit record.
Before the window elapses, prefer the built-in path: have the
counterparty resend their payout invoice, which flips the row back to
`pending-payout` automatically.

### Public exposure

The node advertises its bond policy in the kind-38385 info event
(`src/nip33.rs::info_to_tags`) so clients can warn users before they
trade: `bond_enabled` (always emitted), and when enabled `bond_apply_to`,
`bond_amount_pct`, `bond_base_amount_sats`, `bond_slash_on_waiting_timeout`,
`bond_slash_node_share_pct`, and `bond_payout_claim_window_days`.

## Migrating to a Different Lightning Node

Hold invoices live only in the node that issued them, so a Mostro instance
cannot simply be pointed at a new node while trades are open: every
`release` / `cancel` on a pre-existing order would hit an unknown invoice.
The daemon ships a **maintenance (drain) mode** for this. Full design in
`docs/MAINTENANCE_MODE_LN_MIGRATION.md`; this is the operator procedure.

### When this applies

- **Same LND, new host** (same seed, same `channel.db`): hold invoices travel
  with the node. Nothing here is needed; plan the downtime and go.
- **Different node identity** (new seed, LND → CLN, hosted → self-hosted):
  follow this procedure.

### What is bound to the old node

`GetMaintenanceStatus` (admin RPC, `docs/RPC.md` §8) reports it:

| Counter | Meaning |
|---|---|
| `escrowed_orders` | orders whose hold invoice is still held on the node (non-terminal, not yet settled) |
| `inflight_payouts` | buyer payouts in flight (`settled-hold-invoice` with a payout hash) |
| `unpaid_dev_fees` | successful orders whose dev fee is still unpaid |
| `open_bonds` | bond hold invoices still open (`requested` / `locked`) |
| `pending_bond_payouts` | bonds waiting for, or in the middle of, their payout |
| `pending_orders` | informational only: no escrow, does not block the switch |

`drained` is `true` when every counter except `pending_orders` is zero.

### Procedure

1. **Announce** the window to users. Allow at least
   `max_expiration_days` plus time for open disputes.
2. **Enable maintenance mode** (loopback only; add
   `-H 'authorization: Bearer <token>'` if `[rpc].auth_token` is set):

   ```bash
   grpcurl -plaintext -import-path proto -proto admin.proto \
     -d '{"enabled": true, "reason": "LN node migration"}' \
     127.0.0.1:50051 mostro.admin.v1.AdminService/SetMaintenanceMode
   ```

   From now on `new-order`, `take-buy` and `take-sell` are answered with
   `cant-do` reason `maintenance_mode`; everything on existing orders keeps
   working. The info event is republished at once with
   `maintenance_mode = "true"`, so up-to-date clients warn users before they
   try. The flag is persisted in `daemon_state` and survives restarts.
3. **Drain.** Poll until `drained` is `true`:

   ```bash
   grpcurl -plaintext -import-path proto -proto admin.proto \
     127.0.0.1:50051 mostro.admin.v1.AdminService/GetMaintenanceStatus
   ```

   Meanwhile: pending orders expire on their own (or ask makers to cancel).
   To shorten the drain, cancel them yourself — the `CancelOrder` gRPC
   accepts a `pending` order from the operator and releases the maker's
   bond at once (`grpcurl … AdminService/CancelOrder`, see `docs/RPC.md`;
   `mostro-cli admcancel` goes over Nostr with `ADMIN_NSEC` and only
   works here when that key is the daemon's own). Announce it first, it
   is the user's order. Close long-running disputes with `AdminSettle` /
   `AdminCancel`; keep the
   **old node online the whole time** — it also has to finish in-flight
   payouts and dev fees.
4. **Stop `mostrod`** and back up `mostro.db`.
5. **Edit `[lightning]`** (`lnd_cert_file`, `lnd_macaroon_file`,
   `lnd_grpc_host`) to the new node. Leave `allow_node_change = false`.
6. **Start `mostrod`.** The boot guard sees a new node pubkey with all
   counters at zero, logs `Lightning node changed from … with no open
   escrow; recorded` and stores the new pubkey. If it instead logs
   `REFUSING TO START`, something is still bound to the old node: go back
   to step 3 (see also "Disaster recovery" below).
7. **Disable maintenance mode** (`"enabled": false`). Verify that a test
   order can be created and taken and that the info event shows
   `maintenance_mode = "false"`.
8. Only now decommission the old node (close channels, sweep funds).

Rollback at any step before 6: `SetMaintenanceMode {"enabled": false}`
re-opens the book against the old node; nothing else changed.

### Disaster recovery: old node lost with open escrow

`[lightning].allow_node_change = true` exists for exactly one case: the old
node is gone for good. The daemon **cannot** close the affected rows —
`AdminCancel` / `AdminSettle` talk to the *configured* node and would hit an
unknown invoice. With the daemon stopped:

1. Export the rows behind the non-zero counters; these are the trades to
   reconcile by hand.
2. Funds: an unsettled hold invoice on a dead node pays nobody; its HTLCs
   time out at the CLTV delta and the sats return to whoever paid it. If the
   old seed still exists, restore it elsewhere and close channels. Sats that
   were `settled-hold-invoice` but not paid out belong to the buyer and must
   be paid from the new node manually.
3. Database: mark the orders terminal by hand (`canceled-by-admin` for
   unsettled escrow, `completed-by-admin` after a manual payout) and the
   bonds `failed`, so the counters reach zero.
4. Notify the users involved; the daemon sends nothing for rows changed this
   way.
5. Start against the new node. With the counters at zero the guard accepts
   the change without the override; set `allow_node_change = true` only if a
   row could not be closed and you knowingly leave it unresolved. Turn it
   back off afterwards.

### Reading what happened in the logs

- `Maintenance mode is ON: new orders and takes are rejected` — at boot,
  the persisted flag is set.
- `Maintenance mode: rejecting NewOrder from <trade key>` — a gated request.
- `Maintenance flag changed: republishing mostro info now` — the info
  event went out ahead of its interval.
- `Recorded Lightning node identity <pubkey>` — first boot of the guard.
- `REFUSING TO START: Lightning node changed from …` — see step 6.

## Diagrams
```mermaid
flowchart TD
  A[Create Hold Invoice] --> B[Subscribe Single Invoice]
  B -->|Settled| C[Settle Hold]
  B -->|Cancel/Expire| D[Cancel Hold]
  E[Send Payment] --> F[Track/Stream Updates]
```
