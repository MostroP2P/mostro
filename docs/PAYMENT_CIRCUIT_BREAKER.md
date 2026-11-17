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
   (`src/lightning/mod.rs`, `fn send_payment`). That is where the gate lives.
   Gating callers individually guarantees that whoever adds the next payment
   path forgets one.
2. **Fail closed.** If the ledger cannot be read, the DB is unavailable, or the
   breaker state cannot be determined, the gate **rejects the payment**. A gate
   that fails open is not a gate. This applies with `enforce = true`. In
   observe-only mode nothing is ever rejected, errors included (§5.3).
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
| Trade payout to buyer | `src/app/release.rs`, `fn do_payment` | `release` handler (hot path, user-driven) |
| Dev fee | `src/app/dev_fee.rs`, `fn send_dev_fee_payment` | `fn job_process_dev_fee_payment` (`src/scheduler.rs`) |
| Bond payout to counterparty | `src/app/bond/payout.rs`, `fn pay_counterparty` | `fn job_process_bond_payouts` (`src/scheduler.rs`) |
| Failed and deferred payout retries | `fn job_retry_failed_payments` (`src/scheduler.rs`) → `fn do_payment` | scheduler |
| Dispute resolution payout | `src/app/admin_settle.rs` → release path | admin/solver |

All of them funnel into `LndConnector::send_payment`.

**Halting the scheduler jobs is not sufficient.** The largest outflow — the
trade payout — is driven by a user-supplied Nostr message on the hot path, not
by a job. Jobs are additionally short-circuited (Phase 2) to avoid burning retry
budgets and flooding logs, but that is hygiene, not the control.

`cancel_hold_invoice` is deliberately **not** gated: it releases an HTLC back to
whoever locked it and moves no funds out of the node.

Inflows are recorded at `settle_hold_invoice` call sites — the escrow settle
path (`src/util.rs`, `fn settle_seller_hold_invoice`) and the bond slash paths
(`src/app/bond/slash.rs`, `fn slash_one` and
`fn resolve_range_maker_bond_at_close`).

### 3.1 Existing defenses, and where the breaker sits among them

Several controls on the outflow path already exist. The breaker does not
replace any of them, and none of them makes the breaker redundant: each one
answers a narrower question.

| Control | Where | What it stops | What it does not stop |
|---|---|---|---|
| Duplicate-dispatch guard | `LndConnector::send_payment` | Re-sending a hash LND already reports as in flight or settled | Paying the same order a second time with a **different** invoice; it is advisory and proceeds when the lookup fails or times out |
| At-most-once buyer payout | `fn do_payment` (CAS on `orders.payout_payment_hash`), reconciled by `fn job_reconcile_inflight_payouts` | Two concurrent dispatches of the buyer payout of one order | Outflows other than the buyer payout; a bug that clears or rewrites the marker |
| In-flight payout caps | `fn claim_payout_slot`, `PayoutCaps` (`src/lightning/mod.rs`), used by `fn do_payment` and `fn pay_counterparty` | Too many payouts in flight per destination or node-wide | Amounts; it counts payments, not sats, and **fails open** when the count is unavailable |
| Deferral while LND is unsynced | `fn do_payment` → `fn defer_payout_durably` (`crate::db::mark_payout_deferred`) | Dispatching while LND cannot be trusted | Anything once LND is synced |
| Maintenance mode | `src/app/maintenance.rs`, `MaintenanceState` | New orders and takes while the node drains | Payouts of orders already escrowed, by design |

The breaker adds the one property none of these has: **an amount-based, fail
closed limit that holds across outflow kinds and across restarts.** The
in-flight cap fails open on purpose, because it is a liveness throttle and a
stranded payout is the worse outcome there. The breaker is the safety control,
so it fails closed (principle 2). The two must not share state or failure
policy.

Order of checks on a payout, outermost first:

1. LND-sync deferral and the breaker latch (§5.5) in `fn do_payment` — both
   defer the payout durably instead of failing it.
2. The at-most-once CAS on `payout_payment_hash`.
3. `claim_payout_slot`.
4. Inside `send_payment`: the breaker gate (§5.3), then the duplicate-dispatch
   guard, then LND.

A breaker rejection inside `send_payment` (step 4) is an error like any other
send failure, so the caller's existing failure path releases the in-flight slot
and the payout marker. It must **not** be counted as a failed payment attempt:
an operator who resets the breaker should find the payout still retryable, not
exhausted.

The existing reconciliation of the payout marker treats an `Unknown` or absent
LND record as failed and re-arms the retry. The breaker's ledger keeps that same
payment charged as `unknown` (§5.4), so the re-armed retry is rejected by the
§4.1 invariant. §4.1 defines how that case is handled without tripping the whole
node.

## 4. Detection layers

Two layers, evaluated at different points.

### 4.1 Per-order invariant (synchronous, inside the gate)

> For any order, the principal paid out must never exceed the sats taken in.

```text
Σ out_principal(order_id) + amount ≤ Σ inflow(order_id) + tolerance_sats
```

`out_principal` is the `amount_sats` of every `reserved`, `confirmed` and
`unknown` outflow row for the order. `amount` is the principal of the payment
under evaluation, the `amount` argument of `send_payment`. **Routing fees are
not part of this comparison**; they have their own budget (below and §4.2).

This is the fast, precise layer. Nearly every conceivable draining bug violates
it: paying an order twice, paying an order whose escrow was never settled,
paying more than the escrow held, replaying a payout against a different
invoice. It is checked **before** dispatch, so the bad payment never leaves.

**Why routing fees are kept out.** Routing fees are paid by the node on top of
the principal, so they push a healthy order's outflow *up*, not down. Counting
them would make the invariant false for legitimate trades:

- On a node with `fee = 0` (the `settings.tpl.toml` default) the seller's hold
  invoice is exactly `amount` and the buyer payout is exactly `amount`. Any
  routing fee at all puts outflow above inflow, so every release would trip.
- On a node that does charge a fee, `routing_fee_cap_sats` has a 10-sat floor
  for payments of 1 000 sats or less. A 1 000-sat trade at `fee = 0.006` leaves
  the node 6 sats of margin and reserves a 10-sat cap, so it trips too.
- A bond payout pays the counterparty its share of the slashed bond. With a
  small `slash_node_share_pct` the node keeps less than the routing cap.

On the principal alone, a healthy order satisfies the invariant with
`tolerance_sats = 0` in all three cases. The buyer payout is `amount − buyer
fee`, the dev fee is part of the Mostro fee, and the inflow is
`amount + seller fee`. For a bond, the counterparty share is at most the slashed
amount. Equality is allowed (§4.2 on strict caps), and it is the normal case on
a zero-fee node.

**What leaving fees out costs, and what bounds it.** A payment the invariant
admits can still spend up to `routing_fee_cap_sats(amount)` in routing fees,
because `send_payment` hands LND that value as `fee_limit_sat` and LND enforces
it. A payment can only be admitted while its principal fits the order's inflow,
and a failed payment pays no fee. The fee leak an attacker can extract this way
is therefore bounded by the routing cap of the payments that legitimately fit,
and it is visible in aggregate: the ledger records `fee_sats` on confirmation,
and §4.2 has a rolling `max_routing_fees_sats_per_day` budget plus the
fee-inclusive net-outflow rule.

**Unknown rows on the same order.** A row the reaper parked as `unknown`
(§5.4) keeps its principal charged. If the order's existing payout
reconciliation re-arms a retry for the same payment (§3.1), the retry violates
the invariant. When the violation is **entirely** explained by `unknown` rows of
the same `kind` on that order, that is, when the invariant would hold if those
rows were `failed`, the gate rejects that one payment and alerts the operator
(§5.7)
with the order id and the hashes. It does **not** trip the global latch. The
order stays held until an operator resolves the `unknown` rows (§5.6). Any
other violation trips the breaker. The distinction matters: "LND forgot a
payment" is a per-order question for a human, and "an order is being paid
twice" is a node-wide emergency.

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
| Routing fees in the last 24h exceed cap | `max_routing_fees_sats_per_day` |
| Net outflow (`Σ out − Σ in`) over 24h exceeds cap | `max_net_outflow_sats_24h` |

The outflow, routing-fee and net-outflow aggregates are **fee-inclusive**. An
outflow row contributes `amount_sats + COALESCE(fee_sats,
routing_fee_cap_sats(amount_sats))`, so a row still `reserved` or `unknown`
charges the routing cap, and a `confirmed` row drops back to the fee actually
paid. Confirmation can only release budget. The routing-fee rule sums the same
`COALESCE(fee_sats, routing_fee_cap_sats(amount_sats))` term on its own.

**Every cap is strict (`>`), never `>=`.** A value exactly equal to its
configured cap is allowed; only a value above it trips. A cap is the largest
tolerated value, so `max_payment_sats = 1000000` permits a 1 000 000 sat payment
and trips at 1 000 001. §10 tests both sides of that boundary.

`max_payment_sats` is additionally enforced **synchronously in the gate** — a
single oversized payment must be stopped before it leaves, not detected a minute
later.

The net-outflow rule is the direct expression of the requirement ("more sats
leaving than entering"). In healthy operation the principal balances per
order, and the Mostro fee the node keeps usually covers the routing fees, so a
sustained net outflow well above routing costs is a leak. **On a zero-fee node
the healthy net outflow is not zero.** It equals the routing fees the node pays,
so `max_net_outflow_sats_24h` must sit above the expected daily routing spend.
The Phase 0 numbers are what set it.

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
/// Deliberately carries **no amount**: `send_payment` already takes one, and a
/// second copy here could disagree with it — the ledger would then check one
/// figure while LND sent another. The gate uses the `amount` parameter as the
/// single source of truth and rejects any non-positive value before writing
/// anything, so a negative amount cannot corrupt the outflow aggregates.
pub struct PaymentIntent {
    pub kind: PaymentKind,
    /// Parent/root order id — see §4.1 on range-order bonds.
    pub order_id: Uuid,
    /// Bond id for `BondPayout`, `None` otherwise.
    pub ref_id: Option<Uuid>,
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
    /// Set at boot when `enforce = true` and the §5.8 reconciliation left
    /// gaps it could not fill.
    LedgerIncomplete { order_ids: Vec<Uuid> },
}
```

`PaymentGuard` holds the pool and caches the latch in an `AtomicU8` so the hot
path does not hit the DB just to learn the breaker is closed. The persisted
latch remains the source of truth, and the atomic is a cache refreshed on write
and on boot. The guard follows the `MaintenanceState` pattern
(`src/app/maintenance.rs`): it lives on `AppContext`, it shares one clone with
the RPC server, and it writes the DB first and the atomic second, so a crash in
between leaves the persisted value authoritative.

**Latch writes are serialized.** Every trip and every reset takes one async
mutex inside `PaymentGuard` and holds it across both the DB write and the
atomic update. The atomic is set from the value just committed, never from the
value the caller intended. Without that, a reset could commit `closed` and then
pause, a trip could commit `tripped` and update the atomic, and the resumed
reset would overwrite the atomic with `closed`. The gate would then allow
payments while the persisted latch is tripped. The atomic is only a fast-path
cache, and §5.3 step 4 re-reads the persisted latch anyway, but the cache must
never disagree with the DB in the permissive direction.

The latch is stored in the existing `daemon_state` key/value table
(`migrations/20260901120000_daemon_state.sql`), not in a table of its own:

| Key | Value |
|---|---|
| `circuit_breaker_state` | `"closed"` / `"tripped"` |
| `circuit_breaker_reason` | serialized `TripReason` of the current trip; empty while closed |
| `circuit_breaker_tripped_at` | unix seconds; empty while closed |

A missing key reads as `closed`. Only the explicit value `"tripped"` blocks, so a
fresh database starts closed. A key that is present but unreadable, or a read
that fails, counts as tripped (principle 2).

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
it. Call sites to update: `src/app/release.rs` (`fn do_payment`),
`src/app/dev_fee.rs` (`fn send_dev_fee_payment`), and the bond payout path in
`src/app/bond/`.

Rejections surface as
`MostroInternalErr(ServiceError::LnPaymentError("circuit breaker tripped: …"))`,
reusing the existing variant so no `mostro-core` change is required.

### 5.3 Gate sequence (inside `send_payment`)

1. Read the cached latch. If `Tripped`, log and return an error. No LND call.
   This is the cheap fast path. The authoritative check is in step 4.
2. Reject a non-positive `amount` outright. If `amount > max_payment_sats`, trip
   and reject (strict comparison, §4.2).
3. Decode the invoice to derive its `payment_hash`. A decode failure rejects
   here, before anything has been written.
4. In **one serialized transaction**: re-read the persisted latch from
   `daemon_state` and reject if it is `tripped`; then re-read `Σ in` and the
   principal `Σ out` (including `reserved` and `unknown`) for
   `intent.order_id`, evaluate
   the §4.1 invariant against the principal `amount`, and insert the `reserved`
   ledger row carrying the `payment_hash` from step 3 only if the invariant
   holds. A violation rolls the transaction back and rejects. It also trips the
   breaker, unless §4.1's `unknown`-row exception applies.
5. Dispatch to LND, only after that transaction has committed.
6. Terminal result confirms the row (`confirmed` with the real fee, or `failed`).

Any error in steps 1–4 — DB unavailable, query failure, insert failure —
rejects the payment (principle 2).

**Why step 4 is one transaction.** Reading the totals and inserting the
reservation as two separate statements leaves a window in which two payouts for
the same order each observe the same totals, each conclude there is room, and
both dispatch. This is not hypothetical: the trade payout runs on the `release`
hot path while the dev-fee job pays out against **the same `order_id`**, so the
two can genuinely overlap. The check and the reservation are therefore one
atomic unit — a single `BEGIN IMMEDIATE` transaction serialized per `order_id`
— and dispatch happens only once it has committed. Splitting or reordering
steps 4 and 5 reintroduces exactly the double-payment the invariant exists to
contain.

**Why the latch is read again inside step 4.** Step 1 reads the latch outside
the transaction. A trip committed between step 1 and step 4 would otherwise not
stop that payment, and it would reserve and dispatch after the trip. A trip
writes the latch under a `BEGIN IMMEDIATE` transaction too, so the two
serialize. A payment that committed its reservation before the trip still
dispatches, because it was admitted while the breaker was closed. Every payment
reserved after the trip is rejected.

**Why the hash is derived before reserving.** The reaper of §5.4 reconciles a
stale `reserved` row by looking its `payment_hash` up at LND. A row written
before the hash is known cannot be reconciled by anything, and because reserved
rows count against every budget, such an orphan would charge the order forever
and eventually trip the breaker on its own. Deriving the hash first costs
nothing — decoding is local and touches no network — and guarantees every
reservation is reconcilable. The reservation still precedes any LND dispatch,
so the write-ahead property of principle 4 is unchanged.

#### 5.3.1 Observe-only mode (`enabled = true`, `enforce = false`)

The breaker runs exactly as it would when enforcing, short of stopping
anything:

- The ledger is written in full: `reserved` rows before dispatch, terminal
  states after, inflows at settlement. The reaper runs.
- Every rule is evaluated: the §4.1 invariant and `max_payment_sats`
  synchronously in the gate, and the §4.2 window rules in the watcher job.
- A violation is logged at `warn!`/`error!` and appends a `would-trip` row to
  `circuit_breaker_event`, carrying the full `TripReason`. Rows are written for
  the §4.1 `unknown`-row exception too, as `would-hold`. The operator is alerted
  through §5.7.
- **The payment is dispatched anyway.** The latch is never set, maintenance
  mode is not touched, `payments_paused` stays `false`, and no order is held.
- **Errors do not block either.** A DB or query failure while evaluating is
  logged, recorded in the health event's breaker section as `last_error`, and
  the payment proceeds. Principle 2 (fail closed) applies only with
  `enforce = true`. Otherwise the trial period could stop payouts, which is the
  one thing it promises not to do.

The `would-trip` rows serve two purposes. They are what an operator reviews to
tell false positives from real ones before enforcing, and they are what the
regtest simulation of §10.1 asserts on, instead of parsing logs.

### 5.4 Reserved-row reaper

`send_payment` streams payment updates to a caller-owned listener, so the
confirmation in step 6 happens in the caller. Relying on every caller to
confirm correctly would reintroduce exactly the "someone will forget" problem
this design avoids.

Instead, **a `reserved` row counts against every budget as if it succeeded.**
Confirmation is an accuracy improvement, not a safety requirement. A reaper
inside the watcher job resolves rows still `reserved` after
`reserved_row_timeout_seconds` by calling `LndConnector::lookup_payment`
(`src/lightning/mod.rs`, `fn lookup_payment`) on the stored hash. It also runs
once at boot, so rows stranded by a process stop are resolved before the first
payment of the new process. Until it resolves a row, the conservative
assumption stands.

`lookup_payment` returns a `PaymentLookup`, which today carries the status,
`resolved_at` and the failure reason, but **no fee**. Phase 0 adds
`fee_sat: Option<i64>` to `PaymentLookup`, filled from the `fee_sat` of the
`Payment` that `TrackPaymentV2` already streams, so a succeeded row can be
confirmed with its real fee. If a succeeded payment comes back without a fee,
the row is still `confirmed`, with `fee_sats = NULL`. The aggregates then keep
charging `routing_fee_cap_sats` for it (§4.2), which can only overstate the
outflow.

**Only a definitive terminal answer may clear a reservation.** The lookup has
three outcomes and they are not interchangeable:

| LND result | Row becomes | Why |
|---|---|---|
| `Ok(Some(Succeeded))` | `confirmed`, with `fee_sats` from `PaymentLookup.fee_sat` | Definitive |
| `Ok(Some(Failed))` | `failed`, budget released | Definitive |
| `Ok(Some(InFlight))` | stays `reserved` | Not terminal yet; retry next tick |
| `Ok(None)` | `unknown`, kept charged, operator alerted (§5.7) | LND has no record — see below |
| `Err(_)` | stays `reserved` | Transport failure says nothing about the payment |

`Ok(None)` is the dangerous one. It means LND has no record of the hash, which
covers both "never attempted" and "attempted, succeeded, and the record was
since pruned" — the two are indistinguishable from the daemon's side. Marking
such a row `failed` would release its budget and permit a retry, which is
precisely how a circuit breaker meant to *prevent* double payments would cause
one. It is therefore parked in `unknown`: it keeps counting against every budget
exactly as `reserved` did, and it is surfaced to the operator for manual
resolution rather than resolved by guessing. `unknown` is a terminal state for
the reaper — it never re-attempts the lookup — but it is not a terminal state
for the operator, who can settle it once LND's history or the counterparty
confirms what happened.

### 5.5 Trip actions

1. Persist `tripped` to the `daemon_state` latch keys (§5.1), append a `trip`
   row to `circuit_breaker_event`, and update the atomic cache.
2. `error!` log with the full `TripReason`.
3. Alert the operator through the health event and mostro-watchdog (§5.7).
4. Turn on maintenance mode with the reserved reason `circuit_breaker` (§5.5.1),
   which refuses new orders and takes.
5. Scheduler payment jobs skip their tick (Phase 2).
6. `fn do_payment` checks the latch next to its existing LND-sync check and
   **defers** the payout through `fn defer_payout_durably` instead of
   dispatching it. The order stays settled with its payout pending, it is not
   charged a failed attempt, and `job_retry_failed_payments` picks it up once
   the breaker is reset. That covers `release`, `admin-settle` and the retry
   job with one check. The user receives the same notice a deferred payout
   already produces, not an error.
7. Publish `payments_paused = true` on the NIP-33 info event, from
   `info_tags_from_settings` (`src/nip33.rs`) next to `maintenance_mode`. The
   immediate republish uses the same notify mechanism `MaintenanceState` uses.
   The tag is always emitted, like `maintenance_mode`, so clients can tell
   "not paused" from "older daemon".

Explicitly **not** done on trip: cancelling hold invoices, settling anything,
pausing inbound flows other than order creation (see below), auto-resetting.

New order creation is also refused while tripped. Accepting users into a system
that provably cannot pay them out only widens the blast radius.

#### 5.5.1 Reusing maintenance mode for the order-creation block

Maintenance mode (`src/app/maintenance.rs`) already refuses `NewOrder`,
`TakeBuy` and `TakeSell` at the right place in `accept_event`, persists across
restarts, and is advertised on the info event. The breaker does not build a
second gate. On trip it turns maintenance on with a new reserved reason,
`REASON_CIRCUIT_BREAKER = "circuit_breaker"`, next to `REASON_LND_UNSYNCED`.

Ownership has to be handled with care, because the daemon already lifts its own
maintenance by itself:

- The breaker's maintenance is written with `maintenance_automatic = 0`.
  `disable_if_automatic` lifts any maintenance flagged automatic, whatever its
  reason. It runs when LND has been resynced and in the boot paths of
  `load_for_lightning` / `load_for_cashu`. If the breaker used the automatic
  flag, a resync or a restart would reopen order creation under a tripped
  breaker.
- If the automatic `lnd_unsynced` maintenance is already on when the breaker
  trips, the breaker takes it over: reason `circuit_breaker`, automatic `0`.
- If an operator's maintenance is already on, the breaker leaves it alone.
  Order creation is already refused, and the operator's reason stands.
- `SetMaintenanceMode` refuses the reserved reason `circuit_breaker`, as it
  already refuses `lnd_unsynced`, so that reason always means the breaker.
- Lifting maintenance never clears the breaker. The latch, not maintenance,
  is what stops payouts. An operator who lifts maintenance while the breaker is
  tripped reopens order creation and nothing else, and `SetMaintenanceMode`
  warns when that happens.

That refusal is **Lightning-mode only, and cannot reach a Cashu order.** Escrow
mode is node-wide and exclusive (`Settings::escrow_mode`): a node with `[cashu]`
enabled runs in Cashu mode, where `send_payment` is never called, the watcher
job is never registered (§8, Phase 2), and the breaker consequently can never
trip. The order-creation block is reached only on a Lightning-mode node, where
every order is Lightning-backed by definition. `accept_event` being a shared
prologue for both `run` and `run_cashu` is therefore not a hazard here — but the
check must still be written against the breaker state, which is inert in Cashu
mode, rather than assumed to run only on Lightning nodes.

### 5.6 Reset

Admin-only, via RPC (`proto/admin.proto`, `src/rpc/service.rs`), subject to the
existing admin auth and rate limiting:

- `CircuitBreakerStatus` returns the current state, the reason, `tripped_at`,
  the window aggregates that drove the trip, and the orders held by §4.1's
  `unknown`-row exception.
- `CircuitBreakerReset` clears the latch. It requires an operator note.
  **No timeout-based auto-reset exists.** If maintenance is still on with the
  reason `circuit_breaker`, the reset also lifts it. Maintenance with any other
  reason stays on.
- `CircuitBreakerResolveUnknown` takes a ledger row id, a resolution
  (`confirmed` or `failed`) and an operator note. It is the only way an
  `unknown` row leaves that state, and `failed` is what releases a held order
  for retry.

The latch holds **current state only**; every trip and reset overwrites the
previous one. The audit trail is a separate append-only table,
`circuit_breaker_event` (§6): one row per trip and per reset, never updated. An
operator investigating a drain needs the *first* trip, and a breaker that tripped
twice would otherwise have overwritten exactly the record that mattered.

### 5.7 Operator alerts: health event and mostro-watchdog

Alerts go through the path operators already run for node health. mostrod
publishes the health event (kind 38389, `docs/HEALTH.md`), with its content
NIP-44 encrypted to `[health].watchdog_pubkey`. mostro-watchdog reads it and
alerts on Telegram, only on state changes, with hysteresis and reminders. The
breaker adds a section to that event and the watchdog adds rules for it.
No new channel is built. Other channels can be added later on top of the same
data.

**Public tags.** One new `check`, `circuit_breaker`, Lightning mode only:

| State | When |
|---|---|
| `ok` | Breaker closed, or observe-only |
| `fail` | Latch tripped. `status` becomes `down`: the node takes trades it cannot pay out |
| `unknown` | The latch cannot be read |

The public check deliberately ignores would-trips and held orders. A public
signal that flips when a rule *almost* fires is an oracle: an attacker could
send probe payments and watch it to learn the thresholds, then drain just below
them. A trip is no secret, since `payments_paused` is already public.

**Encrypted content** (`src/app/health/content.rs`), a `circuit_breaker`
object:

- `mode`: `off` | `observe` | `enforce`;
- `state`, `reason` (serialized `TripReason`), `tripped_at`;
- `would_trip_total` and `would_hold_total`: monotonic counts of `would-trip`
  and `would-hold` rows, persisted in `daemon_state` so a restart does not
  reset them;
- `recent`: the last 10 `would-trip` / `would-hold` / `order-held` /
  `ledger-gap` events, each with a monotonic `seq`, its kind, rule, order id,
  observed value and cap;
- `held_orders`: orders held by the §4.1 `unknown`-row exception;
- `headroom`: for each §4.2 window rule, the current value and its cap;
- `last_error`: the last evaluation error in observe-only mode, or `null`.

Monotonic counters are needed because the health event is a periodic
snapshot. The watchdog can count the events between two snapshots even if it
missed one in between.

`recent` is bounded, so a burst can outrun it: 11 would-trips between two
snapshots raise the counter by 11 while `recent` holds 10. The watchdog keeps
the highest `seq` it has alerted on. For each snapshot it sends one detailed
warning per entry in `recent` with a higher `seq`. If the counters rose by more
than the number of such entries, it sends **one aggregate warning** for the
rest: "N more would-trips without details", pointing to `CircuitBreakerStatus`
and the `circuit_breaker_event` table, where every event is kept. A burst
therefore never goes unreported, and it never floods the chat either.

**Republish timing.** A trip and a reset change the public check, so they
publish at once, still at most once every 10 seconds. Would-trips and held
orders do **not** trigger an immediate republish. They wait for the next
regular `[health].interval` (60 s by default). Otherwise the publication time
itself would leak what the encrypted content hides: an event appearing seconds
after a probe payment tells the prober it crossed a rule. A minute of latency
costs nothing during a calibration period.

**mostro-watchdog rules** (companion PR in MostroP2P/mostro-watchdog,
`src/health/rules.rs`):

| Signal | Alert |
|---|---|
| `circuit_breaker` check is `fail` | 🔴 Critical, with the reason; reminders every `renotify_interval`; "resolved" on reset |
| `would_trip_total` increased (observe-only) | 🟡 One warning per new event in `recent`, with rule, order and value against cap, plus one aggregate warning for any excess |
| `would_hold_total` increased (observe-only) | 🟡 Warning per new `would-hold`: this order would have been held for an `unknown` row (same aggregate rule) |
| A new order in `held_orders` | 🟡 Warning with the order id and hashes |
| A new `ledger-gap` event (§5.8) | 🟡 Warning: a payment moved without a ledger row; enforcement is blocked until it is reconciled |
| `last_error` set | 🟡 Warning: the trial period is not evaluating |

**Without a watchdog.** If `watchdog_pubkey` is empty, the content is empty and
the operator has the logs, the `circuit_breaker_event` table and the
`CircuitBreakerStatus` RPC. The runbook (Phase 3) recommends running the
watchdog as part of enabling the breaker, and mostrod logs a startup warning
when `[circuit_breaker]` is enabled without a watchdog key.

The health event's format is specified in the protocol book
(`health_event.md`), so the new check and content fields need a protocol book
update next to the mostrod change.

### 5.8 Ledger completeness before enforcing

The §4.1 invariant only protects an order whose ledger history is complete.
Two situations leave it incomplete, and either one would make enforcement
wrong:

- **Orders that predate the ledger.** An escrow settled before Phase 0 has no
  inflow row. If its buyer payout or dev fee is still pending when enforcement
  starts, the gate compares that payout against an inflow of zero and trips on
  a legitimate payment.
- **Writes lost in observe-only mode.** In observe-only mode a failed ledger
  write never blocks the payment (§5.3.1). The payment can then succeed with no
  outflow row. Once the DB recovers and enforcement is on, the order's inflow is
  visible but its payout is not, and the gate would admit a second payment
  against the same inflow. That one is the dangerous direction.

Both are closed by a **reconciliation pass** that rebuilds missing rows from
the records the daemon already keeps for its own payment flows:

| Ledger row | Authoritative source |
|---|---|
| Escrow inflow | `orders` with a settled hold invoice (`hold_settled_at`, falling back to `invoice_held_at`); amount `amount + fee` |
| Buyer payout | `orders.payout_payment_hash` and the order's success status, confirmed against `lookup_payment` |
| Dev fee | `orders.dev_fee_paid` / `orders.dev_fee_payment_hash` |
| Bond slash inflow and bond payout | `bonds`, including `payout_payment_hash` |

Rows written this way carry `source = 'reconciled'`, so the regular write path
is never confused with a reconstruction. Every outflow they reconstruct is
looked up at LND by hash, and only a `Succeeded` answer becomes `confirmed`.
Anything else becomes `unknown`, which keeps the order charged.

When it runs:

- **In Phase 0**, at boot and after every ledger write failure. Each gap it
  finds and fills appends a `ledger-gap` event, which the watchdog reports.
  Each gap it cannot fill stays listed in `CircuitBreakerStatus`.
- **Before `enforce = true` takes effect**, at boot. If any gap remains
  unresolved, mostrod **starts with the latch tripped**, with reason
  `LedgerIncomplete` and the order ids, rather than enforcing on a ledger it
  knows is wrong. That is fail closed, and it is visible on the watchdog. The
  operator resolves the orders with `CircuitBreakerResolveUnknown` or lets the
  pending payouts finish, then resets.

The same reconciliation runs as the first step of the Phase 0 migration, so a
node that upgrades with settled-but-unpaid orders starts with their inflows on
record.

## 6. Schema

`migrations/<ts>_payment_circuit_breaker.sql`, where `<ts>` is the timestamp of
the Phase 0 PR. It must sort after every migration already on `main`. The latch
itself needs no migration, because it lives in `daemon_state` (§5.1).

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
  -- 'live' (written by the payment path) | 'reconciled' (rebuilt by the
  -- spec §5.8 reconciliation from the orders and bonds tables)
  source           varchar(10) not null default 'live',
  -- 'reserved' | 'confirmed' | 'failed' | 'unknown'. 'reserved' and 'unknown'
  -- both count against every budget as if they had succeeded (spec §5.4).
  state            varchar(10) not null,
  -- Unix timestamps in seconds, matching the rest of the schema.
  created_at       integer not null,
  updated_at       integer not null
);

CREATE INDEX IF NOT EXISTS idx_payment_ledger_order ON payment_ledger(order_id);
CREATE INDEX IF NOT EXISTS idx_payment_ledger_created ON payment_ledger(created_at);
CREATE INDEX IF NOT EXISTS idx_payment_ledger_state ON payment_ledger(state);

-- Inflows are idempotent on the settled invoice's hash. A retry that finds the
-- invoice already settled (the "already settled" branch of the bond slash
-- paths) must not book the same incoming HTLC twice: a duplicated inflow row
-- inflates the per-order allowance and would license a second payout. Writers
-- use INSERT OR IGNORE against this index. Outflows are excluded because a
-- failed payment may legitimately be retried under the same hash.
CREATE UNIQUE INDEX IF NOT EXISTS idx_payment_ledger_inflow_hash
  ON payment_ledger(payment_hash) WHERE direction = 'in' AND payment_hash IS NOT NULL;

-- Append-only audit trail, never updated or deleted. The current latch lives
-- in daemon_state (spec §5.1) and holds current state only; this table is what
-- an operator reads when reconstructing an incident. The first trip is the
-- interesting one, and the latch alone would have overwritten it on the second.
CREATE TABLE IF NOT EXISTS circuit_breaker_event (
  id               char(36) primary key not null,
  -- 'trip' | 'reset' | 'order-held' | 'unknown-resolved', and in
  -- observe-only mode 'would-trip' | 'would-hold' (spec §5.3.1), and
  -- 'ledger-gap' from the reconciliation (spec §5.8)
  event            varchar(16) not null,
  -- Serialized TripReason for 'trip'; the order id and hashes for
  -- 'order-held'; the ledger row id and resolution for 'unknown-resolved';
  -- NULL for 'reset'.
  reason           text,
  -- Operator note for 'reset' and 'unknown-resolved'; NULL otherwise.
  note             text,
  created_at       integer not null
);

CREATE INDEX IF NOT EXISTS idx_circuit_breaker_event_created
  ON circuit_breaker_event(created_at);
```

Example rows (synthetic) for a completed trade of 50 000 sats at `fee = 0.004`
(100 sats from each side) and `dev_fee_percentage = 0.30`:

```text
id                                    direction kind          order_id   amount_sats fee_sats state      created_at
7f1c…-…-0001                          in        escrow-settle 3a9e…-0007       50100     NULL confirmed  1786500000
7f1c…-…-0002                          out       trade-payout  3a9e…-0007       49900       12 confirmed  1786500004
7f1c…-…-0003                          out       dev-fee       3a9e…-0007          60        1 confirmed  1786500061
```

Principal out is 49 960, which is ≤ 50 100 in, so the §4.1 invariant holds. The
13 sats of routing fees count only toward the §4.2 aggregates.

The same trade on a zero-fee node:

```text
id                                    direction kind          order_id   amount_sats fee_sats state      created_at
7f1c…-…-0004                          in        escrow-settle 3a9e…-0008       50000     NULL confirmed  1786500100
7f1c…-…-0005                          out       trade-payout  3a9e…-0008       50000       12 confirmed  1786500104
```

Principal out equals inflow, so the invariant holds at equality. The node is
12 sats down on this order, and those sats count toward the routing-fee and
net-outflow rules.

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
# Slack allowed on the per-order invariant (sats). The invariant compares
# principal only (routing fees are budgeted separately), and a healthy order's
# principal out never exceeds its inflow, so 0 is correct, including on a
# zero-fee node where the two are equal.
tolerance_sats = 0
# Also enforced synchronously in the gate, not only by the watcher.
max_payment_sats = 1000000
max_outflow_sats_per_hour = 5000000
max_outflow_sats_per_day = 20000000
max_payments_per_hour = 200
# Routing fees paid (or reserved at their cap) over a rolling 24h window. The
# per-order invariant does not count fees, so this is what bounds them.
max_routing_fees_sats_per_day = 50000
# Ceiling on (outflow - inflow), fee-inclusive, over a rolling 24h window. This
# is the direct "more sats leaving than entering" rule. On a zero-fee node the
# healthy value equals the routing spend, so keep this above
# max_routing_fees_sats_per_day.
max_net_outflow_sats_24h = 1000000
# How long a ledger row may sit 'reserved' before the reaper resolves it
# against LND. It counts against every budget until then.
reserved_row_timeout_seconds = 300
```

Validating deserializers reject the following at startup, following the
`slash_node_share_pct` precedent in `src/config/types.rs` — a typo that disables
a safety limit must stop the daemon, not silently widen it:

- non-positive caps (`max_payment_sats`, the outflow and count caps,
  `max_routing_fees_sats_per_day`, `max_net_outflow_sats_24h`);
- negative `tolerance_sats`;
- non-positive `check_interval_seconds` — zero would busy-loop the watcher job;
- non-positive `reserved_row_timeout_seconds` — zero would make every
  reservation instantly stale, so the reaper would race live payments to LND.

## 8. Phases

Each phase is one PR.

### Phase 0 — Ledger, write-ahead, observe-only

- Migration, `[circuit_breaker]` config block, `src/circuit_breaker.rs`.
- `PaymentIntent` threaded through `send_payment` and all call sites.
- Ledger rows written on both sides: `reserved`/`confirmed` at outflow points,
  `in` rows at the `settle_hold_invoice` call sites of §3.
- `fee_sat` added to `PaymentLookup` (§5.4).
- `job_circuit_breaker_watch` registered in `src/scheduler.rs`
  (`fn start_scheduler`), inside the `!Settings::is_cashu_enabled()` block
  alongside the other Lightning-only jobs. It evaluates the §4.2 window rules
  and runs the reserved-row reaper, also once at boot. Without the watcher,
  Phase 0 could not collect the window-rule numbers it exists to collect.
- The §5.8 reconciliation, run by the migration, at boot and after every
  ledger write failure, with `ledger-gap` events.
- All rules evaluate and log. **Nothing is rejected. The latch is never set.**
  Violations append `would-trip` / `would-hold` rows, and evaluation errors
  never block (§5.3.1).
- The `circuit_breaker` section of the health event (§5.7): the public check
  and the encrypted content.
- Companion PRs: the protocol book (`health_event.md`) for the new check and
  content fields, and mostro-watchdog for the §5.7 rules, so the trial period
  alerts on Telegram from day one.

Ships dark and safe. Its purpose is to produce real numbers for §7.

**Gate to Phase 1:** the §10.1 regtest simulation records zero `would-trip`
and zero `would-hold` rows across the whole scenario catalogue and all four
configuration variants, and every positive control is caught.

### Phase 1 — Per-order invariant, persistent latch, live gate, reset

- The `daemon_state` latch keys (§5.1) plus the `AtomicU8` cache, loaded at
  boot, with serialized latch writes.
- §4.1 invariant and `max_payment_sats` enforced synchronously in the gate,
  including the latch re-check inside the reservation transaction and the
  `unknown`-row exception that holds one order without tripping.
- `enforce = true` becomes meaningful: violations trip and reject.
- Fail-closed behavior on DB/query errors.
- The §5.8 pre-enforcement check: unresolved ledger gaps start the daemon
  tripped with `LedgerIncomplete`.
- `fn do_payment` defers the payout through `fn defer_payout_durably` while the
  latch is tripped (§5.5), so no payout is lost or charged a failed attempt by
  the gate going live.
- `CircuitBreakerStatus` / `CircuitBreakerReset` /
  `CircuitBreakerResolveUnknown` in `proto/admin.proto` and
  `src/rpc/service.rs`. **The phase that can trip must also be able to
  recover**: shipping the latch without its authenticated reset would leave
  an operator with no supported way out of a Phase 1 trip.

### Phase 2 — Window rules enforced, propagation

- The §4.2 window rules, already evaluated since Phase 0, now trip when
  `enforce = true`.
- `job_process_dev_fee_payment`, `job_process_bond_payouts`, and
  `job_retry_failed_payments` skip their tick while tripped.
- Trip turns on maintenance mode with the reserved reason `circuit_breaker`,
  with the ownership rules of §5.5.1. `SetMaintenanceMode` refuses that reason.

### Phase 3 — Client signal and runbook

- `payments_paused` tag on the NIP-33 info event.
- Operator runbook section in `docs/LIGHTNING_OPS.md`, covering how to set up
  mostro-watchdog for the breaker, how to read a `would-trip` alert during the
  trial period, and how to resolve a trip, a held order, a ledger gap and an
  `unknown` row.

## 9. Failure modes and calibration

- **False positives.** Routing fees, unusual bond timing, and legitimate
  large trades all move the aggregates. Routing fees cannot trip the per-order
  invariant (§4.1 leaves them out on purpose), but they do feed the routing-fee
  and net-outflow rules. This is why Phase 0 is observe-only and
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
  Phases 0–3. The escrow seam `EscrowBackend` (`src/escrow.rs`) is the natural
  place for that follow-up: handlers already go through it, so it can play the
  role `send_payment` plays here. That work should be tracked separately rather
  than assumed.

## 10. Test plan

Per phase, co-located Rust unit tests:

- Ledger: inflow/outflow rows written on both sides; `reserved` counted as
  spent; reaper resolves stale rows via a mocked `lookup_payment_status`.
- Reaper status handling (§5.4 table): `Succeeded` confirms with the real fee;
  `Failed` releases the budget; `InFlight` and `Err(_)` leave the row
  `reserved`; `Ok(None)` moves it to `unknown` and it **keeps counting against
  every budget** — the regression test asserts that a pruned-but-succeeded
  payment is never released for retry.
- Inflow idempotency: replaying a settled invoice (the "already settled" retry
  branch) writes exactly one `in` row, and the per-order allowance is unchanged
  by the replay.
- Invariant: payout principal equal to inflow passes; principal exceeding
  inflow by one sat trips; a second payout against an already-paid order trips;
  range-order bond child payout keyed on the parent order does **not** trip.
- Fee exclusion (regression for the zero-fee default): with `fee = 0`, a full
  trade (settle `amount`, pay out `amount`) passes the invariant even though
  `routing_fee_cap_sats(amount)` is positive. A 1 000-sat trade at
  `fee = 0.006`, where the 10-sat routing floor exceeds the node's margin,
  passes. A bond payout with `slash_node_share_pct = 0` passes.
- Unknown-row exception: an order whose only excess comes from an `unknown` row
  of the same kind is held (one payment rejected, an `order-held` event, admins
  notified) and the latch stays closed. An excess that remains after the
  `unknown` rows are discounted trips. `CircuitBreakerResolveUnknown` with
  `failed` releases the held order, and with `confirmed` keeps it held.
- Routing-fee budget: reserved rows charge `routing_fee_cap_sats`, confirmed
  rows charge `fee_sats`, and `max_routing_fees_sats_per_day` trips one sat
  above its cap.
- Latch races: a reset and a trip interleaved at every await point never leave
  the cache `closed` while the DB says `tripped`. A trip committed between
  step 1 and step 4 of §5.3 rejects the payment in step 4.
- Reaper fee: a succeeded lookup confirms the row with `PaymentLookup.fee_sat`.
  A succeeded lookup without a fee confirms it with `fee_sats = NULL`, and the
  aggregates keep charging the routing cap. A row left `reserved` by a
  simulated crash is resolved by the boot pass before the first payment.
- Reconciliation (§5.8): an order settled before the ledger existed gets its
  inflow from `orders` and its pending payout passes. A payout whose ledger
  write failed in observe-only mode is rebuilt from `payout_payment_hash` and a
  `Succeeded` lookup, so a second payment against the same inflow trips. A gap
  the pass cannot fill starts an enforcing daemon tripped with
  `LedgerIncomplete`.
- Watchdog counters: 11 would-trips between two snapshots produce 10 detailed
  warnings and one aggregate warning. A `would-hold` raises
  `would_hold_total` and produces its own warning.
- Concurrency: two payouts for the same `order_id` submitted simultaneously —
  exactly one reserves and dispatches, the other is rejected. Asserts the §5.3
  check-and-reserve transaction is genuinely atomic.
- Latch: trip persists across a simulated restart; no code path clears it except
  the admin reset; reset requires a note.
- Audit trail: two consecutive trips leave two `circuit_breaker_event` rows, and
  the first one is still readable after the second.
- Fail-closed: a failing pool makes the gate reject rather than allow.
- Observe-only: with `enforce = false`, a violating payment is logged, writes a
  `would-trip` row and is still dispatched; the latch stays closed, maintenance
  is untouched and no order is held. A failing pool in observe-only mode lets
  the payment through and sets `last_error`.
- Health event: the `circuit_breaker` check is `ok` in observe-only mode even
  after would-trips, `fail` when tripped (and `status` is `down`), and absent in
  Cashu mode. The encrypted content carries `would_trip_total`, which survives a
  restart. A would-trip does not trigger an immediate republish; a trip does.
- Config validation: zero or negative `check_interval_seconds` and
  `reserved_row_timeout_seconds` are rejected at startup.
- Velocity: each rule of §4.2 passes at exactly its cap and trips one unit
  above it (strict `>`, per §4.2).
- Propagation: while tripped, the payment jobs no-op, `release` defers the
  payout through `defer_payout_durably` without spending a payment attempt, and
  the info event carries `payments_paused`. After a reset, the deferred payout
  is dispatched by `job_retry_failed_payments`.
- Coexistence (§3.1): a breaker rejection inside `send_payment` releases the
  in-flight slot taken by `claim_payout_slot` and the `payout_payment_hash`
  marker. With the slot gate failing open (count unavailable), the breaker
  still rejects.
- Maintenance interplay (§5.5.1): a trip turns maintenance on with reason
  `circuit_breaker` and automatic `0`. Neither an LND resync nor a restart
  lifts it. A trip during the automatic `lnd_unsynced` maintenance takes it
  over. An operator's maintenance is left untouched. `SetMaintenanceMode`
  refuses the reason `circuit_breaker`. Lifting maintenance leaves the latch
  tripped, and `CircuitBreakerReset` lifts only maintenance whose reason is
  still `circuit_breaker`.
- Disabled: with no `[circuit_breaker]` block, no ledger rows are written and
  every existing test stays green.

### 10.1 Regtest simulation

Unit tests prove the logic against mocks. The question they cannot answer is
whether **real** Lightning behavior makes the breaker fire on legitimate
traffic: real routing fees, real retries, real LND records, real timing. That
is answered on regtest before any phase can enforce.

**Harness.** Use [Mortsom](https://github.com/MostroP2P/mortsom) rather than a
new one. `mortsom stack up` brings up a regtest chain, three LND nodes (alice,
bob, carol), a relay and a mostrod built from `main`; `--profile cli` drives
trades through mostro-cli with no app; the scenario catalogue already covers
the money paths (`happy`, `bond`, `dispute`, `cancellation`, `expiration`,
`payment_failure`, `recovery`); and the daemon's settings come from
`stack/mostro-settings.tpl.toml`.

**Method.** Run the whole catalogue with `[circuit_breaker] enabled = true,
enforce = false`, then assert on `circuit_breaker_event`: **zero `would-trip`
and zero `would-hold` rows**. A non-empty table names the rule, the order and
the values, which is the false positive to fix before enforcing.

**Configuration variants.** The fee bug this spec once had was a function of
configuration, so the catalogue runs under each of these:

| Variant | What it exercises |
|---|---|
| `fee = 0` | The template default: the invariant holds at exact equality |
| `fee = 0.006`, trades of 1 000 sats or less | The 10-sat routing floor above the node's margin |
| Bonds on, `slash_node_share_pct = 0` | A bond payout equal to the slashed inflow |
| Range orders | Parent versus child keys on payouts and bonds |

**Breaker-specific scenarios** the catalogue does not force today:

- **Real routing fees.** Route the buyer payout through carol (multi-hop) so
  `fee_sats` is non-zero and the routing-fee and net-outflow rules see real
  numbers.
- **LND forgets a payment.** After a payout, delete its record
  (`lncli deletepayments`). The reaper must park the row as `unknown`, a
  re-armed retry must be held for that order only, and the node-wide latch must
  stay closed.
- **Concurrency.** A `release` and the dev-fee job on the same order at the
  same moment. Both legitimate payments pass, and nothing is double-counted.
- **Restart mid-payout.** Kill mostrod between the `reserved` row and the
  terminal result. After restart, the reaper resolves the row from LND.
- **"Already settled" replay.** A bond slash retried after LND settled it books
  one inflow, not two.
- **Dispute settled by a solver** through `admin-settle`.

**Positive controls.** A breaker that never fires also reports zero
false positives. A test-only cargo feature in mostrod (for example
`circuit-breaker-fault-injection`, never built into releases) injects
draining bugs on purpose:

- pay the same order's buyer payout a second time with a different invoice;
- pay out an order whose escrow was never settled;
- inflate a payout above the escrowed amount.

With `enforce = true`, each scenario must see the breaker trip before the bad
payment leaves, and Mortsom's wallet observer must show that the receiving
wallet got **nothing** from it. With `enforce = false`, the same scenarios must
produce exactly one `would-trip` row each and a watchdog alert, which also
tests the §5.7 path end to end.

**What regtest cannot settle.** The §4.2 window thresholds depend on real
volume, and regtest traffic is synthetic. The simulation validates the logic:
the invariant, the reaper, the latch and its interplay with maintenance mode.
The thresholds come only from Phase 0 running on real nodes. A green
simulation must not be read as calibrated thresholds.
