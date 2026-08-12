# Payment Circuit Breaker — Implementation Spec

> Damage containment for a hypothetical fund-draining bug. This document is the
> single source of truth as the feature is rolled out across several PRs. Each
> phase below maps to one **small, atomic PR** that can be reviewed, tested, and
> released independently.
>
> Not to be confused with the per-provider circuit breaker in
> [PRICE_PROVIDERS.md](./PRICE_PROVIDERS.md) §6.5, which trips on a failing
> price API and has nothing to do with fund movement.

## 1. Goal

If an attacker finds a bug that lets them pull sats out of the node, Mostro must
**notice that more sats are leaving than entering and halt every outgoing
payment on its own**, without operator intervention, so the operator has time to
investigate instead of waking up to a drained node.

This is **containment, not prevention**. A draining bug remains a draining bug;
the circuit breaker only bounds how much leaves before a human can react. It is
worth building precisely because the worst realistic scenario is not "we lost
sats" but "we lost sats all night while nobody was watching".

## 2. Guiding principles

1. **Gate at the lowest chokepoint, not at the callers.** Every Lightning
   outflow in the daemon passes through `LndConnector::send_payment`
   (`src/lightning/mod.rs:227`). That is where the gate lives. Gating callers
   individually guarantees that whoever adds the next payment path forgets one.
2. **Fail closed.** If the ledger cannot be read, the DB is unavailable, or the
   breaker state cannot be determined, the gate **rejects the payment**. A gate
   that fails open is not a gate.
3. **The latch is persistent and manual-reset only.** Once tripped, the state
   survives a daemon restart and only an admin can clear it. A breaker that
   resets on restart is exactly the condition an attacker will induce.
4. **Write-ahead accounting.** The ledger row is written *before* the payment is
   dispatched, and an unconfirmed row counts against the budget. A double-pay
   bug is then caught by the gate on the second attempt rather than discovered
   in the post-mortem.
5. **Freeze, never unwind.** Tripping halts outgoing payments. It does **not**
   cancel hold invoices, does not settle anything, and does not attempt any
   corrective transfer. Mass irreversible action under a tripped breaker is the
   last thing anyone wants automated.
6. **Observe before enforcing.** Phase 0 ships in observe-only mode so operators
   can calibrate thresholds against real traffic before the gate can reject a
   legitimate payment.
7. **Tests accompany every phase.** Rust unit tests co-located with the module;
   `cargo test`, `cargo fmt`, `cargo clippy --all-targets --all-features` must
   stay green.

## 3. Outflow surface

Every point where sats leave the node today:

| Outflow | Call site | Trigger |
|---|---|---|
| Trade payout to buyer | `src/app/release.rs:589` | `release` handler (hot path, user-driven) |
| Dev fee | `src/app/dev_fee.rs:993` | `job_process_dev_fee_payment` (`src/scheduler.rs:1087`) |
| Bond payout to counterparty | `src/app/bond/slash.rs`, `src/app/bond/flow.rs` | `job_process_bond_payouts` (`src/scheduler.rs:1121`) |
| Failed-payment retries | `job_retry_failed_payments` (`src/scheduler.rs:213`) | scheduler |
| Dispute resolution payout | `src/app/admin_settle.rs` → release path | admin/solver |

All of them funnel into `LndConnector::send_payment`.

**Halting the scheduler jobs is not sufficient.** The largest outflow — the
trade payout — is driven by a user-supplied Nostr message on the hot path, not
by a job. Jobs are additionally short-circuited (Phase 2) to avoid burning retry
budgets and flooding logs, but that is hygiene, not the control.

`cancel_hold_invoice` is deliberately **not** gated: it releases an HTLC back to
whoever locked it and moves no funds out of the node.

Inflows are recorded at `settle_hold_invoice` call sites — the escrow settle
path (`src/util.rs:1330`) and the bond slash paths (`src/app/bond/slash.rs:798`,
`src/app/bond/slash.rs:1115`).

## 4. Detection layers

Two layers, evaluated at different points.

### 4.1 Per-order invariant (synchronous, inside the gate)

> For any order, the sats paid out must never exceed the sats taken in.

```
Σ outflow(order_id) + pending_amount ≤ Σ inflow(order_id) + tolerance_sats
```

This is the fast, precise layer. Nearly every conceivable draining bug violates
it: paying an order twice, paying an order whose escrow was never settled,
paying more than the escrow held, replaying a payout against a different
invoice. It is checked **before** dispatch, so the bad payment never leaves.

The direction is naturally safe: routing fees and the node fee mean a healthy
order always has `out < in`. Default `tolerance_sats = 0`.

**Bond ledger key.** Range-order bonds carry both `order_id` and
`child_order_id` (see `migrations/20260423120000_anti_abuse_bond.sql`). The
ledger **must** key on the parent/root `order_id` for both the slash inflow and
the payout outflow, otherwise a legitimate child payout looks like an outflow
against an order with zero inflow and trips the breaker.

### 4.2 Velocity and net outflow (asynchronous, watcher job)

Rolling-window aggregates over the ledger, evaluated every
`check_interval_seconds`:

| Rule | Config key |
|---|---|
| Single payment exceeds cap | `max_payment_sats` |
| Outflow in the last hour exceeds cap | `max_outflow_sats_per_hour` |
| Outflow in the last 24h exceeds cap | `max_outflow_sats_per_day` |
| Payment count in the last hour exceeds cap | `max_payments_per_hour` |
| Net outflow (`Σ out − Σ in`) over 24h exceeds cap | `max_net_outflow_sats_24h` |

`max_payment_sats` is additionally enforced **synchronously in the gate** — a
single oversized payment must be stopped before it leaves, not detected a minute
later.

The net-outflow rule is the direct expression of the requirement ("more sats
leaving than entering"). Because inflow and outflow are roughly balanced
per-order in healthy operation, a sustained positive net outflow is a leak.

### 4.3 Deliberately out of scope: balance reconciliation against LND

An earlier draft included a third layer that compared LND's
`ChannelBalance`/`WalletBalance` against what the internal ledger says should be
there. **It is excluded from this spec by decision.**

The cost of that exclusion should be recorded honestly: both remaining layers
are derived from Mostro's own accounting, so **if the draining bug lives in the
accounting itself, the breaker is blind to it**. A bug that moves sats without
ever writing a `payment_ledger` row is not detected by any rule here. The gate
placement in §2.1 is what mitigates this — a payment that does not go through
`send_payment` is the only way to bypass the ledger, and no such path exists
today. Adding one must be treated as a security-relevant change.

Note that the per-payment `lookup_payment_status` call used by the reaper
(§5.4) is *not* this layer — it queries the status of one known payment hash,
not the node's balance.

## 5. Design

### 5.1 New module: `src/circuit_breaker.rs`

```rust
/// What kind of outflow a payment represents. Recorded on the ledger row so
/// the operator can tell at a glance which subsystem leaked.
pub enum PaymentKind { TradePayout, DevFee, BondPayout }

/// Context the gate needs that `send_payment`'s bolt11 + amount cannot supply.
pub struct PaymentIntent {
    pub kind: PaymentKind,
    /// Parent/root order id — see §4.1 on range-order bonds.
    pub order_id: Uuid,
    /// Bond id for `BondPayout`, `None` otherwise.
    pub ref_id: Option<Uuid>,
    pub amount_sats: i64,
}

pub enum BreakerState { Closed, Tripped { reason: TripReason, tripped_at: i64 } }

pub enum TripReason {
    OrderInvariant { order_id: Uuid, out: i64, inflow: i64 },
    PaymentTooLarge { amount: i64, cap: i64 },
    HourlyOutflow { total: i64, cap: i64 },
    DailyOutflow { total: i64, cap: i64 },
    HourlyCount { count: i64, cap: i64 },
    NetOutflow { net: i64, cap: i64 },
    /// Set by an admin via RPC. Lets an operator halt payments manually.
    Manual { note: String },
}
```

`PaymentGuard` holds the pool and caches the latch in an `AtomicU8` so the hot
path does not hit the DB just to learn the breaker is closed. The DB row remains
the source of truth; the atomic is a cache refreshed on write and on boot.

### 5.2 `send_payment` signature change

```rust
pub async fn send_payment(
    &mut self,
    payment_request: &str,
    amount: i64,
    intent: &PaymentIntent,          // new
    listener: Sender<PaymentMessage>,
) -> Result<(), MostroError>
```

Threading an explicit intent through every call site is intentional: it makes it
impossible to add a new outflow without stating what it is and which order funds
it. Call sites to update: `src/app/release.rs:589`, `src/app/dev_fee.rs:993`,
and the bond payout path in `src/app/bond/`.

Rejections surface as
`MostroInternalErr(ServiceError::LnPaymentError("circuit breaker tripped: …"))`,
reusing the existing variant so no `mostro-core` change is required.

### 5.3 Gate sequence (inside `send_payment`, before `decode_invoice`)

1. Read the latch. If `Tripped`, log and return an error. No LND call.
2. If `amount > max_payment_sats`, trip and reject.
3. Query `Σ in` / `Σ out` (including `reserved`) for `intent.order_id`. If the
   §4.1 invariant would be violated, trip and reject.
4. Insert the ledger row as `reserved`.
5. Dispatch to LND.
6. Terminal result confirms the row (`confirmed` with the real fee, or `failed`).

Any error in steps 1–4 — DB unavailable, query failure, insert failure —
rejects the payment (principle 2).

In observe-only mode (`enforce = false`), steps 1–4 still evaluate and log at
`warn!`/`error!`, and step 4 still writes the row, but nothing is rejected and
the latch is not set.

### 5.4 Reserved-row reaper

`send_payment` streams payment updates to a caller-owned listener, so the
confirmation in step 6 happens in the caller. Relying on every caller to
confirm correctly would reintroduce exactly the "someone will forget" problem
this design avoids.

Instead, **a `reserved` row counts against every budget as if it succeeded.**
Confirmation is an accuracy improvement, not a safety requirement. A reaper
inside the watcher job resolves rows still `reserved` after
`reserved_row_timeout_seconds` by calling
`LndConnector::lookup_payment_status` (`src/lightning/mod.rs:325`) on the stored
hash, and marks them `confirmed` or `failed`. Until it does, the conservative
assumption stands.

### 5.5 Trip actions

1. Persist `Tripped` to `circuit_breaker_state` and update the atomic cache.
2. `error!` log with the full `TripReason`.
3. Notify admins over the existing message queue.
4. Scheduler payment jobs skip their tick (Phase 2).
5. `release` and `admin-settle` handlers reject early with a clear user-facing
   message, rather than letting the request travel to LND to fail there.
6. Publish `payments_paused = true` on the NIP-33 info event (`src/nip33.rs`,
   alongside the existing `bond_*` policy tags) so clients can warn users.

Explicitly **not** done on trip: cancelling hold invoices, settling anything,
pausing inbound flows other than order creation (see below), auto-resetting.

New order creation is also refused while tripped. Accepting users into a system
that provably cannot pay them out only widens the blast radius.

### 5.6 Reset

Admin-only, via RPC (`proto/admin.proto`, `src/rpc/service.rs`), subject to the
existing admin auth and rate limiting:

- `CircuitBreakerStatus` — current state, reason, `tripped_at`, and the
  window aggregates that drove it.
- `CircuitBreakerReset` — clears the latch. Requires an operator note, which is
  persisted for the audit trail. **No timeout-based auto-reset exists.**

## 6. Schema

`migrations/20260811120000_payment_circuit_breaker.sql`:

```sql
-- Append-only record of every sat entering or leaving the node.
CREATE TABLE IF NOT EXISTS payment_ledger (
  id               char(36) primary key not null,
  -- 'in' (hold invoice settled) | 'out' (payment sent)
  direction        varchar(3) not null,
  -- 'trade-payout' | 'dev-fee' | 'bond-payout' | 'escrow-settle' | 'bond-slash'
  kind             varchar(16) not null,
  -- Parent/root order id. Range-order bond rows key on the parent, never the
  -- child — see spec §4.1.
  order_id         char(36) not null,
  -- Bond id for bond rows, NULL otherwise.
  ref_id           char(36),
  amount_sats      integer not null,
  -- Actual routing fee, known only after a payment confirms. NULL otherwise.
  fee_sats         integer,
  payment_hash     char(64),
  -- 'reserved' | 'confirmed' | 'failed'. A 'reserved' row counts against every
  -- budget as if it had succeeded (spec §5.4).
  state            varchar(10) not null,
  -- Unix timestamps in seconds, matching the rest of the schema.
  created_at       integer not null,
  updated_at       integer not null
);

CREATE INDEX IF NOT EXISTS idx_payment_ledger_order ON payment_ledger(order_id);
CREATE INDEX IF NOT EXISTS idx_payment_ledger_created ON payment_ledger(created_at);
CREATE INDEX IF NOT EXISTS idx_payment_ledger_state ON payment_ledger(state);

-- Single-row latch. Persistent so a restart cannot clear a trip.
CREATE TABLE IF NOT EXISTS circuit_breaker_state (
  id               integer primary key check (id = 1),
  -- 'closed' | 'tripped'
  state            varchar(8) not null,
  -- Serialized TripReason. NULL while closed.
  reason           text,
  tripped_at       integer,
  -- Operator note supplied at reset time, retained as an audit trail.
  reset_note       text,
  reset_at         integer,
  updated_at       integer not null
);

INSERT OR IGNORE INTO circuit_breaker_state (id, state, updated_at)
  VALUES (1, 'closed', 0);
```

Example rows (synthetic) for a completed trade of 50 000 sats:

```
id                                    direction kind          order_id   amount_sats fee_sats state      created_at
7f1c…-…-0001                          in        escrow-settle 3a9e…-0007       50100     NULL confirmed  1786500000
7f1c…-…-0002                          out       trade-payout  3a9e…-0007       50000       12 confirmed  1786500004
7f1c…-…-0003                          out       dev-fee       3a9e…-0007          50        1 confirmed  1786500061
```

## 7. Configuration

New optional `[circuit_breaker]` block. Absent block = feature off, byte-for-byte
today's behavior. `src/config/types.rs`, template in `settings.tpl.toml`,
documented in `docs/STARTUP_AND_CONFIG.md`.

```toml
[circuit_breaker]
# Master switch. Off = no ledger writes, no gate, no watcher job.
enabled = true
# false = observe-only: evaluate and log, never reject and never latch.
# Run this way first to calibrate the thresholds below against real traffic.
enforce = false
# Watcher job cadence (seconds).
check_interval_seconds = 60
# Slack allowed on the per-order invariant (sats). 0 is correct in normal
# operation, since routing fees make a healthy order's outflow strictly
# smaller than its inflow.
tolerance_sats = 0
# Also enforced synchronously in the gate, not only by the watcher.
max_payment_sats = 1000000
max_outflow_sats_per_hour = 5000000
max_outflow_sats_per_day = 20000000
max_payments_per_hour = 200
# Ceiling on (outflow - inflow) over a rolling 24h window. This is the direct
# "more sats leaving than entering" rule.
max_net_outflow_sats_24h = 1000000
# How long a ledger row may sit 'reserved' before the reaper resolves it
# against LND. It counts against every budget until then.
reserved_row_timeout_seconds = 300
```

Validating deserializers reject non-positive caps and negative tolerances at
startup, following the `slash_node_share_pct` precedent in
`src/config/types.rs` — a typo that disables a safety limit must stop the
daemon, not silently widen it.

## 8. Phases

Each phase is one PR.

### Phase 0 — Ledger, write-ahead, observe-only

- Migration, `[circuit_breaker]` config block, `src/circuit_breaker.rs`.
- `PaymentIntent` threaded through `send_payment` and all call sites.
- Ledger rows written on both sides: `reserved`/`confirmed` at outflow points,
  `in` rows at the `settle_hold_invoice` call sites of §3.
- All rules evaluate and log. **Nothing is rejected. The latch is never set.**

Ships dark and safe. Its purpose is to produce real numbers for §7.

### Phase 1 — Per-order invariant, persistent latch, live gate

- `circuit_breaker_state` read/write plus the `AtomicU8` cache, loaded at boot.
- §4.1 invariant and `max_payment_sats` enforced synchronously in the gate.
- `enforce = true` becomes meaningful: violations trip and reject.
- Fail-closed behavior on DB/query errors.

### Phase 2 — Velocity rules, watcher job, propagation

- `job_circuit_breaker_watch` registered in `start_scheduler`
  (`src/scheduler.rs:26`), inside the `!Settings::is_cashu_enabled()` block
  alongside the other Lightning-only jobs.
- Rolling-window rules of §4.2 plus the reserved-row reaper.
- `job_process_dev_fee_payment`, `job_process_bond_payouts`, and
  `job_retry_failed_payments` skip their tick while tripped.
- `release` / `admin-settle` handlers and order creation reject early with a
  user-facing message.

### Phase 3 — Operator surface

- `CircuitBreakerStatus` / `CircuitBreakerReset` in `proto/admin.proto` and
  `src/rpc/service.rs`.
- Admin notification on trip.
- `payments_paused` tag on the NIP-33 info event.
- Operator runbook section in `docs/LIGHTNING_OPS.md`.

## 9. Failure modes and calibration

- **False positives.** Routing fees, unusual bond timing, and legitimate
  large trades all move the aggregates. This is why Phase 0 is observe-only and
  why `max_payment_sats` should be set relative to `max_order_amount`, not
  guessed. A tripped breaker is an outage; the thresholds must be earned from
  data.
- **Detection latency versus damage.** A watcher on a 60-second cadence gives an
  attacker a 60-second window. That is exactly why the per-order invariant and
  the single-payment cap are enforced *synchronously in the gate*, not by the
  job. The job catches the slow, distributed leak; the gate catches the fast one.
- **The blind spot.** Restated from §4.3 because it matters: every rule here is
  computed from Mostro's own ledger. A bug that moves funds without a ledger row
  is invisible to all of them.
- **Cashu mode is not covered.** The gate is Lightning-only. Cashu outflows
  (`src/cashu/`) pass through no equivalent chokepoint and are out of scope for
  Phases 0–3. Extending containment there is a follow-up and should be tracked
  separately rather than assumed.

## 10. Test plan

Per phase, co-located Rust unit tests:

- Ledger: inflow/outflow rows written on both sides; `reserved` counted as
  spent; reaper resolves stale rows via a mocked `lookup_payment_status`.
- Invariant: payout equal to inflow passes; payout exceeding inflow by one sat
  trips; a second payout against an already-paid order trips; range-order bond
  child payout keyed on the parent order does **not** trip.
- Latch: trip persists across a simulated restart; no code path clears it except
  the admin reset; reset requires a note.
- Fail-closed: a failing pool makes the gate reject rather than allow.
- Observe-only: with `enforce = false`, a violating payment is logged and still
  dispatched, and the latch stays closed.
- Velocity: each rule of §4.2 trips at its threshold and not below it.
- Propagation: while tripped, the payment jobs no-op, `release` rejects, and the
  info event carries `payments_paused`.
- Disabled: with no `[circuit_breaker]` block, no ledger rows are written and
  every existing test stays green.
