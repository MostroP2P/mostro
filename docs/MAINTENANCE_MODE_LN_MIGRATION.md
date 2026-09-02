# Maintenance Mode & Lightning Node Migration — Implementation Spec

**Status:** Implemented · **Target:** `main` (`mostro-core` ≥ 0.14.6) ·
**Feature flag:** none at build time; the mode is a runtime switch, off by
default

Implementation trail: Phase 0 — mostro-core#166 (0.14.6), protocol#57 ·
Phase 1 — #933 · Phase 2 — #934 · Phase 3 — #935 · Phase 4 — #936 ·
Phase 5 — operator runbook in `docs/LIGHTNING_OPS.md` ("Migrating to a
Different Lightning Node"). Where this document and the code differ, the
code and `docs/RPC.md` / `docs/LIGHTNING_OPS.md` are authoritative.

This document specifies a **maintenance ("drain") mode** for `mostrod` and the
operational procedure that uses it to move a Mostro instance from one
Lightning node to a different one without stranding open trades.

---

## 1. Context — why a plain node swap breaks live orders

### 1.1 What is bound to the node

Every trade that has progressed past `pending` holds escrow as a **hold
invoice** on the Lightning node. `mostrod` stores a copy of the payment hash
and the preimage in `orders.hash` / `orders.preimage`
(`migrations/20221222153301_orders.sql`), but the invoice itself, its HTLCs
and its accept/settle/cancel state live only inside the node's database.

The daemon later drives that invoice through `EscrowBackend`
(`src/escrow.rs`):

| Daemon action | Node call (`src/lightning/mod.rs`) | Fails on a different node? |
|---|---|---|
| Take flow → `show_hold_invoice` (`src/util.rs`) | `create_hold_invoice` | n/a — new invoice |
| `Release`, `AdminSettle` | `settle_hold_invoice(preimage)` | **yes** — unknown invoice |
| `Cancel`, `AdminCancel`, expiry jobs | `cancel_hold_invoice(hash)` | **yes** |
| Boot / resubscribe | `subscribe_invoice(hash)` | **yes** |
| Buyer payout, dev fee, bond payout | `send_payment` + `track_payment_v2(hash)` | payouts start fresh, but in‑flight tracking breaks |

A hold invoice cannot be exported from one node and imported into another.
Once the daemon is pointed at a new node, every `release`/`cancel` on a
pre‑existing order returns a gRPC error and the order is stuck.

### 1.2 Same node, new host ≠ new node

If the operator is only relocating **the same** LND (same seed, same
`channel.db`) to a new machine, hold invoices travel with the node and none
of this document applies — plan the downtime and go. This spec targets the
case where the daemon will talk to a **different** node identity (new seed,
LND → CLN, hosted → self‑hosted, etc.).

### 1.3 Inventory of node-bound state

The exact rows that must reach a terminal state on the **old** node before
the switch:

| Set | Table / predicate | Why |
|---|---|---|
| A. Escrowed orders | `orders.hash IS NOT NULL AND status NOT IN (terminal)` | hold invoice must be settled or canceled on the node that issued it |
| B. In‑flight buyer payouts | `orders.payout_payment_hash IS NOT NULL AND status = 'settled-hold-invoice'` | `job_reconcile_inflight_payouts` tracks the hash on the node that sent it. Same predicate as `find_inflight_payouts` (`src/db.rs`); a subset of A, reported separately for visibility |
| C. Unpaid dev fees | `orders.dev_fee > 0 AND dev_fee_paid = 0 AND status = 'success'` | `job_process_dev_fee_payment` pays from the node; harmless to re‑run on the new node but cleaner to drain |
| D. Open bond hold invoices | `bonds.hash IS NOT NULL AND state IN ('requested','locked')` | maker/taker bond hold invoices (`src/app/bond/flow.rs`) |
| E. Pending bond payouts | `bonds.state = 'pending-payout'` | payout not yet sent or in flight; `bonds.payout_payment_hash` is the idempotency record for it |

Predicate notes, verified against the schema and jobs:

- The bonds table column is `state`, not `status`
  (`migrations/20260423120000_anti_abuse_bond.sql`). Its values are
  `requested`, `locked`, `released`, `pending-payout`, `slashed`, `forfeited`,
  `failed`.
- `bonds.payout_payment_hash` is **never cleared**: `slash_after_success`
  (`src/app/bond/payout.rs`) moves the row to `slashed` and keeps the hash as
  its idempotency record. Counting `payout_payment_hash IS NOT NULL` alone
  would therefore never reach zero on an instance that has ever paid a
  slashed bond. E keys on `state` for that reason.
- `orders.payout_payment_hash` can likewise be left behind on a terminal
  order when the post‑finalisation clear loses to a DB error;
  `find_inflight_payouts` ignores that residue unless the status is
  `settled-hold-invoice`, and so does B.

`terminal` for orders is: `canceled`, `canceled-by-admin`, `settled-by-admin`,
`completed-by-admin`, `expired`, `success`, `cooperatively-canceled`.

Everything else — `pending` orders without a bond, users, disputes metadata,
reputation — is node‑agnostic and survives the switch untouched.

### 1.4 Approach

Introduce a daemon‑wide **maintenance mode**:

- while enabled, `mostrod` refuses to *open new escrow* (create or take an
  order) and tells the client why;
- everything that *closes* escrow keeps working (release, cancel, dispute
  resolution, admin settle/cancel, expiry jobs, payouts);
- the admin RPC exposes a **drain status** with the counters from §1.3;
- when every counter is zero the operator stops the daemon, changes
  `[lightning]` in `settings.toml`, and starts it against the new node.

The alternative of routing per order to one of two nodes was considered and
rejected: it doubles the escrow surface for a one‑off operational event.

---

## 2. Requirements

### 2.1 Functional

- **R1** — An admin can enable/disable maintenance mode at runtime without
  restarting `mostrod`.
- **R2** — The flag survives a daemon restart (drain windows can last days;
  restarts must not silently re‑open the book).
- **R3** — With the flag on, `NewOrder`, `TakeBuy` and `TakeSell` are
  rejected with a dedicated `CantDoReason::MaintenanceMode` so clients can
  show a specific message.
- **R4** — With the flag on, every other action keeps its current behaviour.
  In particular `Release`, `Cancel`, `FiatSent`, `AddInvoice`,
  `AddBondInvoice`, `Dispute`, `RateUser`, `AdminCancel`, `AdminSettle`,
  `AdminTakeDispute`, `Orders`, `RestoreSession`, `TradePubkey`,
  `LastTradeIndex` are **not** gated.
- **R5** — The scheduler keeps running unchanged: pending‑order expiry,
  waiting‑payment cancellation, failed‑payment retries, in‑flight payout
  reconciliation, dev‑fee and bond payouts all continue to drain state.
- **R6** — The admin RPC returns the counters of §1.3 so the operator can
  tell when the drain is complete.
- **R7** — The Mostro info event (NIP‑33, `src/nip33.rs::info_to_tags`)
  carries a `maintenance_mode` tag so clients can warn users *before* they
  try to post an order.
- **R8** — On boot, the daemon persists the node's `identity_pubkey`; if it
  differs from the stored one **and** any §1.3 counter is non‑zero, the
  daemon refuses to start unless explicitly overridden. This turns the
  "wrong node" failure from a silent stuck‑order bug into a loud startup
  error.

### 2.2 Non-functional

- No change in behaviour when the flag is off (default). Existing tests must
  pass unmodified.
- Cashu mode (`run_cashu`) is out of scope: it has no Lightning escrow. The
  gate is still applied there for consistency, but the node‑identity guard
  (R8) is skipped.
- No new external dependencies.

### 2.3 Out of scope

- Multi‑node routing.
- Automatic migration of channels or funds between nodes.
- Client UI; only the protocol surface (reason + info tag) is defined here.

---

## 3. Design

### 3.1 Persistence — `daemon_state` table

A minimal key/value table, migration `migrations/<ts>_daemon_state.sql`:

```sql
CREATE TABLE IF NOT EXISTS daemon_state (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
```

Keys used by this spec:

| Key | Value | Written by |
|---|---|---|
| `maintenance_mode` | `"0"` / `"1"` | admin RPC (§3.4) |
| `maintenance_reason` | free text, may be empty | admin RPC |
| `maintenance_since` | unix seconds | admin RPC |
| `ln_node_pubkey` | hex identity pubkey from `LnStatus.node_pubkey` | boot (§3.6) |

A tiny accessor module `src/app/daemon_state.rs` exposes
`get(pool, key) -> Option<String>` and `set(pool, key, value)`. No ORM
struct is needed.

Rationale for DB over `settings.toml`: the switch must be flipped by an
operator over RPC while the process runs, and the config file is loaded once
at boot into an immutable `Arc<Settings>`.

### 3.2 In-memory handle — `MaintenanceState`

```rust
// src/app/maintenance.rs
#[derive(Clone)]
pub struct MaintenanceState {
    enabled: Arc<AtomicBool>,
}

impl MaintenanceState {
    pub async fn load(pool: &SqlitePool) -> Result<Self, MostroError>; // reads daemon_state
    pub fn is_enabled(&self) -> bool;                                 // Ordering::Acquire
    pub async fn set(&self, pool: &SqlitePool, enabled: bool, reason: Option<&str>)
        -> Result<(), MostroError>;                                   // writes DB, then flips the atomic
}
```

`AppContext` (`src/app/context.rs`) gains a `maintenance: MaintenanceState`
field with a `maintenance()` accessor, built in `main.rs` right after the DB
pool. The same clone is handed to the RPC server so both sides observe one
flag. `set` writes the DB first and flips the atomic second, so a crash
between the two leaves the *persisted* value authoritative on the next boot.

### 3.3 The gate

The three creating/taking actions are dispatched in
`handle_message_action_no_ln` (`src/app.rs`, arms `Action::NewOrder`,
`Action::TakeSell`, `Action::TakeBuy`) and, in Cashu mode, in
`dispatch_cashu`. Add one check **before** those arms:

```rust
if ctx.maintenance().is_enabled()
    && matches!(action, Action::NewOrder | Action::TakeBuy | Action::TakeSell)
{
    return Err(MostroError::MostroCantDo(CantDoReason::MaintenanceMode).into());
}
```

The existing error path (`manage_errors` → `enqueue_cant_do_msg`) already
delivers the `CantDo` to the sender, so no new messaging code is needed.

Placement note: the gate runs in `accept_event` **before**
`check_trade_index`, and after PoW, the spam gate, the outer signature check
and decryption. The reason is that `check_trade_index` is not read‑only: for
an identity it has never seen that carries a trade index it calls
`add_new_user` and persists that index (`src/app.rs`, the `Err(_)` arm of
`is_user_present`). A first‑time user whose `NewOrder` is rejected by a gate
placed *after* that call would have their first index burned, and the same
signed request retried after maintenance ends would fail with
`InvalidTradeIndex`. Gating first means a rejected request persists nothing.
The gate still needs the decrypted action and the sender key, both of which
`accept_event` has at that point; it does not need the inner signature, since
the only side effect is a `CantDo` reply to the visible sender.

What the gate deliberately does **not** block:

- `AddBondInvoice` / `PayBondInvoice` — these belong to an order that already
  exists (`waiting-maker-bond` / `waiting-taker-bond`). Blocking them would
  strand a bond hold invoice instead of letting it settle or expire.
- `AddInvoice` — buyer providing the payout invoice for an already escrowed
  order.
- Any admin action.

### 3.4 Admin RPC

Extend `proto/admin.proto`:

```proto
service AdminService {
  // ...existing rpcs...
  rpc SetMaintenanceMode(SetMaintenanceModeRequest) returns (SetMaintenanceModeResponse);
  rpc GetMaintenanceStatus(GetMaintenanceStatusRequest) returns (GetMaintenanceStatusResponse);
}

message SetMaintenanceModeRequest {
  bool enabled = 1;
  optional string reason = 2;
  optional string request_id = 3;
}
message SetMaintenanceModeResponse {
  bool success = 1;
  optional string error_message = 2;
}

message GetMaintenanceStatusRequest { optional string request_id = 1; }

message DrainCounters {
  uint32 escrowed_orders        = 1; // §1.3 A
  uint32 inflight_payouts       = 2; // §1.3 B
  uint32 unpaid_dev_fees        = 3; // §1.3 C
  uint32 locked_bonds           = 4; // §1.3 D
  uint32 inflight_bond_payouts  = 5; // §1.3 E
  uint32 pending_orders         = 6; // informational: pending, no escrow
}

message GetMaintenanceStatusResponse {
  bool enabled = 1;
  optional string reason = 2;
  optional int64 since = 3;          // unix seconds
  DrainCounters counters = 4;
  bool drained = 5;                  // all of A..E == 0
  string ln_node_pubkey = 6;         // node currently connected
  optional string stored_ln_node_pubkey = 7; // from daemon_state
}
```

Implementation lands in `src/rpc/service.rs` next to the existing handlers.
`AdminServiceImpl` gains a `maintenance: MaintenanceState` field and its
constructor takes it (called from `main.rs`).

The counters are one SQL function `drain_counters(pool) -> DrainCounters` in
`src/app/maintenance.rs`, six `SELECT COUNT(*)` statements using the
predicates in §1.3 literally. `drained` is `A+B+C+D+E == 0`; `pending_orders`
is reported but not part of `drained` because pending orders hold no escrow.

**Authorisation.** The admin gRPC has no authentication interceptor today,
and the `RateLimiter` in `AdminServiceImpl` is applied only inside
`validate_db_password`, not to every method. The existing mutating RPCs
(`CancelOrder`, `SettleOrder`, `AddSolver`) already run under that model,
relying on the default loopback bind. `SetMaintenanceMode` must not widen
the exposure: the handler rejects any call whose `remote_addr` is not a
loopback address with `PermissionDenied`, using the same `remote_addr`
extraction `validate_db_password` already does. `GetMaintenanceStatus` is
read‑only and is not restricted. A proper authenticated interceptor for the
whole admin service is a pre‑existing gap and is tracked separately; it is
not a prerequisite for this feature.

### 3.5 Info event tag

`info_to_tags` (`src/nip33.rs`) is called from `job_info_event_send`
(`src/scheduler.rs`), which currently takes only `&LnStatus`. Add a
`maintenance: bool` argument and emit:

```text
["maintenance_mode", "true" | "false"]
```

Always emitted (like `bond_enabled`) so clients can tell "old daemon" from
"maintenance off". The job re‑publishes every
`publish_mostro_info_interval` seconds; after `SetMaintenanceMode` the RPC
handler also triggers one immediate publish so clients see the change
without waiting a full interval. Simplest mechanism: a `tokio::sync::Notify`
stored next to the atomic in `MaintenanceState`, awaited in the job loop
with `select!` against the interval sleep.

### 3.6 Node identity guard (boot)

In `main.rs`, after `LN_STATUS.set(ln_status)`:

1. Read `daemon_state.ln_node_pubkey`.
2. If absent → store `ln_status.node_pubkey`, continue.
3. If equal → continue.
4. If different → compute `drain_counters`. If `drained` → log a warning,
   overwrite the stored pubkey, continue. Otherwise **exit non‑zero** with a
   message listing the non‑zero counters and the two pubkeys.

Override: `[lightning].allow_node_change = true` (default `false`, added to
`settings.tpl.toml` and `LightningSettings`) skips step 4's exit and only
logs. It exists for disaster recovery only: the old node is gone for good and
the operator accepts that the affected rows **cannot be resolved by the
daemon**. `AdminCancel` and `AdminSettle` are not a recovery path here: both
call `cancel_hold_invoice` / `settle_hold_invoice` on the *currently
configured* node before touching the order, so they fail with an
unknown‑invoice error against the new node. See §5.1 for the manual
procedure. The flag must never be left on in normal operation; the wizard
does not ask for it.

### 3.7 Behaviour matrix while enabled

| Situation | Behaviour |
|---|---|
| Client sends `NewOrder` / `TakeBuy` / `TakeSell` | `CantDo(MaintenanceMode)`; nothing persisted |
| Client releases / cancels an escrowed order | unchanged |
| Pending order reaches `expiration` | `job_expire_pending_older_orders` expires it as today |
| `waiting-payment` seller never pays | `job_cancel_orders` cancels the hold as today |
| Dispute opened / resolved | unchanged; solver uses `AdminSettle`/`AdminCancel` |
| Maker bond outstanding (`waiting-maker-bond`) | maker can still pay it; on payment the order is published as `pending` and simply cannot be taken |
| Daemon restart | flag reloaded from `daemon_state`; still enabled |
| RPC `GetMaintenanceStatus` | live counters |

---

## 4. Phases

Each phase is one PR, independently reviewable and shippable. Phase N+1
depends on Phase N unless stated otherwise.

### Phase 0 — Protocol surface (`mostro-core` + protocol book)

**Deliverables**

- `mostro-core`: add `CantDoReason::MaintenanceMode` (`src/error.rs`), with
  serde name `maintenance_mode`, and its `Display` text
  (`"Mostro is in maintenance mode and is not accepting new orders"`).
- `mostro-core`: add a `#[serde(other)] Unknown` catch-all to `CantDoReason`
  so that clients on this release tolerate reasons added later (today an
  unknown reason fails deserialisation of the whole `CantDo` payload).
- Protocol book: document the new reason and the `maintenance_mode` info tag.
- Cut a `mostro-core` minor release; bump `Cargo.toml` in `mostrod` in Phase 1.

**Tests** — serde round trip of the new variant, matching the existing
`CantDoReason` tests in `mostro-core`.

**Exit criteria** — release published on crates.io.

### Phase 1 — Persistent flag + gate (`mostrod`)

**Deliverables**

- Migration `daemon_state` (§3.1).
- `src/app/daemon_state.rs` accessors.
- `src/app/maintenance.rs` with `MaintenanceState` (§3.2) and
  `drain_counters` (§3.4).
- `AppContext.maintenance` wired in `main.rs` (both Lightning and Cashu boot
  paths).
- Gate in `handle_message_action_no_ln` and `dispatch_cashu` (§3.3).
- `Cargo.toml`: `mostro-core` bump to the Phase 0 release.

**Tests** (RED first, then GREEN)

- `maintenance_state_round_trips_through_daemon_state` — `set(true)` then
  fresh `load` reports enabled.
- `gate_rejects_new_order_when_enabled` /
  `gate_rejects_take_buy_when_enabled` /
  `gate_rejects_take_sell_when_enabled` — using the `create_test_message`
  helpers already in `src/app.rs`; assert the queued `CantDo` carries
  `MaintenanceMode` and no order row was inserted.
- `gate_passes_release_cancel_fiat_sent_when_enabled` — table‑driven over
  the R4 list; assert dispatch reaches the handler (mock/no‑op handler as
  in existing tests) rather than erroring.
- `gate_is_noop_when_disabled` — the whole existing `src/app.rs` test suite
  is the regression net here.
- `drain_counters_reflects_each_predicate` — insert one fixture row per §1.3
  set and assert the matching counter is exactly 1 while the others are 0;
  add a terminal‑status order with `hash` and `payout_payment_hash` set and a
  `slashed` bond with `payout_payment_hash` set, and assert neither is
  counted.
- `gate_rejects_first_time_user_without_creating_it` — unknown identity with
  trade index 1 sends `NewOrder` under maintenance; assert the `CantDo` and
  that no `users` row was inserted.

**Exit criteria** — `cargo test`, `cargo clippy --all-targets --all-features`,
`cargo fmt --check` green; coverage on the two new modules ≥ 80 %.

### Phase 2 — Admin RPC

**Deliverables**

- `proto/admin.proto` additions (§3.4); `build.rs` already compiles the
  file.
- `SetMaintenanceMode` / `GetMaintenanceStatus` handlers in
  `src/rpc/service.rs`; `AdminServiceImpl::new` takes `MaintenanceState`;
  loopback‑only check on `SetMaintenanceMode` (§3.4).
- `docs/RPC.md` and `docs/ADMIN_RPC_AND_DISPUTES.md`: new methods, example
  `grpcurl` invocations.

**Tests**

- `set_maintenance_mode_persists_and_flips_flag` — call RPC, assert both the
  atomic and `daemon_state` changed.
- `get_maintenance_status_reports_counters` — seed one escrowed order, assert
  `escrowed_orders == 1` and `drained == false`; settle it, assert `drained`.
- `set_maintenance_mode_rejects_non_loopback_peer` — request with a
  non‑loopback `remote_addr` returns `PermissionDenied` and leaves the flag
  untouched; a loopback peer succeeds.
- Existing `offline_service()` test scaffold in `src/rpc/service.rs` is
  reused.

**Exit criteria** — same gates as Phase 1; manual `grpcurl` smoke test
recorded in the PR.

### Phase 3 — Client signalling (info event)

**Deliverables**

- `info_to_tags(ln_status, maintenance)` emits `maintenance_mode` (§3.5).
- `job_info_event_send` reads the flag each tick and wakes on `Notify`.
- `SetMaintenanceMode` handler signals the `Notify`.

**Tests**

- `info_tags_include_maintenance_mode_false_by_default` and
  `_true_when_enabled` — extend the existing `info_to_tags` tests in
  `src/nip33.rs`.
- `info_job_republishes_on_notify` — under `tokio::time::pause()`, as the
  existing scheduler tests do, assert a second event is sent immediately
  after the notify without advancing the interval.

**Exit criteria** — gates green; `mostro-cli`/client maintainers notified of
the new tag (issue link in PR).

Phase 3 is independent of Phase 2 and may be reviewed in parallel; it only
needs Phase 1.

### Phase 4 — Node identity guard

**Deliverables**

- `daemon_state.ln_node_pubkey` written at boot; mismatch check (§3.6).
- `[lightning].allow_node_change` in `LightningSettings`, `settings.tpl.toml`
  and `docs/STARTUP_AND_CONFIG.md`.
- Startup error message names the stuck counters and points to this
  document.

**Tests**

- `node_guard_stores_pubkey_on_first_boot`.
- `node_guard_accepts_same_pubkey`.
- `node_guard_rejects_new_pubkey_with_open_escrow` — expects the boot helper
  to return `Err`; the check is factored into a pure function
  `check_node_identity(stored, current, counters, allow_override) -> Result<Decision>`
  so it is unit‑testable without LND.
- `node_guard_allows_new_pubkey_when_drained`.
- `node_guard_override_only_logs`.

**Exit criteria** — gates green. Because this phase can prevent the daemon
from starting, the PR must include a rollback note (set
`allow_node_change = true` or fix the `[lightning]` block).

### Phase 5 — Operator runbook & release

**Deliverables**

- New section "Migrating to a different Lightning node" in
  `docs/LIGHTNING_OPS.md` (procedure below, §5).
- `docs/README.md` index entry for this spec.
- Tag a `mostrod` release (the repo's `CHANGELOG.md` is the release
  verification guide, not a change log; the release notes carry the entry).

**Exit criteria** — a full dry run on regtest/Polar: two LND nodes, several
orders in every §1.3 state, enable maintenance, drain, switch, verify new
orders work and the guard rejects a switch back with open escrow.

---

## 5. Operator procedure (goes into `LIGHTNING_OPS.md`)

1. Announce the window to users; give at least the configured
   `max_expiration_days` plus dispute lead time.
2. `SetMaintenanceMode{enabled: true, reason: "LN node migration"}`.
   Verify the info event now shows `maintenance_mode = true`.
3. Poll `GetMaintenanceStatus` until `drained == true`. Meanwhile:
   - pending orders expire on their own, or the operator asks makers to
     cancel;
   - long‑running disputes are closed with `AdminSettle` / `AdminCancel`;
   - keep the **old** node online the entire time — it also has to finish
     in‑flight payouts (B/E) and dev fees (C).
4. Stop `mostrod`. Take a backup of `mostro.db`.
5. Edit `[lightning]` (`lnd_cert_file`, `lnd_macaroon_file`,
   `lnd_grpc_host`) to the new node. Leave `allow_node_change = false`.
6. Start `mostrod`. The node guard sees a new pubkey with all counters at
   zero, logs the change and stores the new pubkey.
7. `SetMaintenanceMode{enabled: false}`. Verify a test order can be created
   and taken, and that the info event shows `maintenance_mode = false`.
8. Only now decommission the old node (close channels, sweep funds).

Rollback at any step before 6: `SetMaintenanceMode{enabled: false}` re‑opens
the book against the old node; nothing else changed.

### 5.1 Disaster recovery — old node lost with open escrow

This is the only situation in which `allow_node_change` should be used. The
daemon cannot close the affected rows because every daemon‑side path goes
through the node that no longer exists. The procedure is manual and must be
done **before** enabling `allow_node_change`, with the daemon stopped:

1. List the rows with the §1.3 predicates (A, D and E) and export them; they
   are the trades the operator has to reconcile by hand.
2. Funds: an unsettled hold invoice on a dead node is not paid out to anyone.
   Its HTLCs time out at the CLTV delta and the sats return to the party that
   paid it (the seller or the bonded user). If the old node's seed still
   exists, restoring it elsewhere and closing channels recovers the balance;
   otherwise nothing further can be done on chain. Sats that were already
   `settled-hold-invoice` but not paid out (set B) belong to the buyer and
   must be paid from the new node manually.
3. Database: mark the affected orders terminal by hand
   (`status = 'canceled-by-admin'` for unsettled escrow,
   `'completed-by-admin'` after a manual payout) and the bonds `failed`, so
   the counters reach zero and the guard accepts the new node.
4. Notify the users involved through the usual admin channel; the daemon
   sends no message for rows changed this way.
5. Start the daemon with the new `[lightning]` block. With the counters at
   zero the guard accepts the change without the override; set
   `allow_node_change = true` only if a row could not be closed and the
   operator knowingly leaves it unresolved.

A daemon‑side `ForceCloseOrder` RPC that updates the row without a node call
would make step 3 safer. It is deliberately **not** part of this spec; it can
be proposed once the drain path has shipped and an operator has actually
needed it.

---

## 6. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Drain never reaches zero because of a stuck dispute | `AdminSettle` / `AdminCancel` are not gated and work while the old node is up; counters name the exact rows (§3.4) so the operator can find them |
| Old node lost before the drain finished | §5.1 manual procedure; `allow_node_change` documented as a knowing acceptance of unresolved rows, not a fix |
| `SetMaintenanceMode` reachable from the network | loopback‑only check on the mutating RPC (§3.4); pre‑existing admin‑RPC auth gap tracked separately |
| Operator forgets the flag and the book stays closed | `maintenance_since` in status; info tag visible to every client; log a warning at boot and every info‑event tick while enabled |
| Restart during drain re‑opens the book | flag persisted in `daemon_state` (R2) |
| Daemon started against the wrong node with open escrow | Phase 4 guard exits non‑zero (R8) |
| Old clients don't know `MaintenanceMode` | `CantDoReason` has no catch-all variant today, so an old client fails to parse the `CantDo` payload and shows nothing. Phase 0 adds a `#[serde(other)] Unknown` variant for future additions, and the maintenance window must only be enabled after the client releases that ship the Phase 0 core; the info tag is additive and safe regardless |
| Rejected `NewOrder` burns a trade index | gate runs after validation, before the handler that persists the index (§3.3); covered by a test |
| Trade‑pubkey rotation (#811) racing the gate | rotation is not gated and has no escrow; no interaction |

---

## 7. Open questions

1. Should `SetMaintenanceMode` optionally expire all `pending` orders in one
   call (`expire_pending: bool`) to shorten the drain? Proposed answer: no
   for v1 — `job_expire_pending_older_orders` and makers cancelling cover
   it, and an explicit bulk expiry is easy to add later.
2. Should the `maintenance_reason` be published in the info event? Proposed
   answer: no — free text from the operator in a public event is a footgun;
   clients can show a generic message.
3. Do we want a `--maintenance` CLI flag for boot‑time enabling before the
   RPC is reachable? Proposed answer: not needed; the RPC starts before the
   nostr loop (`main.rs`), and the DB flag persists.

---

## 8. File map

| File | Phase | Change |
|---|---|---|
| `mostro-core/src/error.rs` | 0 | `CantDoReason::MaintenanceMode` |
| `migrations/<ts>_daemon_state.sql` | 1 | new table |
| `src/app/daemon_state.rs` | 1 | new |
| `src/app/maintenance.rs` | 1 | new (`MaintenanceState`, `drain_counters`) |
| `src/app/context.rs` | 1 | `maintenance` field + accessor |
| `src/app.rs` | 1 | gate in `handle_message_action_no_ln`, `dispatch_cashu` |
| `src/main.rs` | 1, 4 | build state; node guard |
| `Cargo.toml` | 1 | `mostro-core` bump |
| `proto/admin.proto` | 2 | two RPCs, `DrainCounters` |
| `src/rpc/service.rs`, `src/rpc/server.rs` | 2 | handlers, constructor |
| `docs/RPC.md`, `docs/ADMIN_RPC_AND_DISPUTES.md` | 2 | docs |
| `src/nip33.rs` | 3 | `maintenance_mode` tag |
| `src/scheduler.rs` | 3 | `job_info_event_send` notify |
| `src/config/types.rs`, `settings.tpl.toml` | 4 | `allow_node_change` |
| `docs/STARTUP_AND_CONFIG.md` | 4 | knob docs |
| `docs/LIGHTNING_OPS.md`, `docs/README.md` | 5 | runbook, index |
