# Anti-Abuse Bond — Implementation Spec

> Implementation guide for [issue #711](https://github.com/MostroP2P/mostro/issues/711).
> This document is the single source of truth as the feature is rolled out across
> several PRs. Each phase below maps to one **small, atomic PR** that can be
> reviewed, tested, and released independently.

## 1. Goal

Add an **opt-in, node-level** anti-abuse bond. A taker (and later a maker) locks
a Lightning hold invoice when entering a trade. The bond is:

- **Released** on normal completion or any cancellation that happens before a
  timeout elapses.
- **Slashed** (claimed by Mostro and paid to the counterparty) only in clearly
  defined conditions configured by the node operator:
  - `slash_on_lost_dispute = true` → the bonded party loses the dispute.
  - `slash_on_waiting_timeout = true` → the bonded party lets the waiting-state
    timeout actually elapse.

Disabled by default. Nodes that don't flip `enabled = true` see zero behavior
change.

## 2. Guiding principles

1. **Opt-in and off-by-default.** Every phase must preserve today's behavior
   when `[anti_abuse_bond].enabled = false`.
2. **Atomic PRs.** Each phase below is scoped to one coherent change that can
   ship (and revert) independently. No phase leaves the daemon in an incoherent
   state.
3. **Never slash unless the cause is unambiguous.** The worst failure mode is
   stealing a good user's bond. When in doubt, release.
4. **Bond is separate from escrow.** A second hold invoice, its own row, its
   own lifecycle. It must not be deducted from or conflated with trade escrow.
5. **Tests accompany every phase.** Rust unit tests co-located with the module;
   `cargo test`, `cargo clippy --all-targets --all-features`, and
   `cargo sqlx prepare -- --bin mostrod` must stay green.

## 3. Configuration surface (final shape)

Add a new optional section to `settings.toml`. Missing section ≡
`enabled = false`.

```toml
[anti_abuse_bond]
# Master switch. When false the bond code paths are dead.
enabled = false

# Bond amount = max(amount_pct * order_amount_sats, base_amount_sats)
# amount_pct is a unitless fraction of the order amount (0.01 = 1%).
amount_pct = 0.01
# Floor, in sats. Prevents trivial bonds on small orders.
base_amount_sats = 1000

# Which flows require the bond.
#   "take"   → only the taker posts a bond
#   "create" → only the maker posts a bond
#   "both"   → both sides
apply_to = "take"

# Slash conditions. Both default to false so enabling the feature with
# an incomplete config never slashes anyone by accident.
slash_on_lost_dispute  = true
slash_on_waiting_timeout = false

# Bond payout retries (applies once the bond is slashed and Mostro needs
# a payout invoice from the winning counterparty).
payout_invoice_window_seconds = 300
payout_max_retries           = 5
```

**`apply_to` is deliberately expressive** so phases can be rolled out one side
at a time on production nodes — operators can keep `apply_to = "take"` until
Phase 5 has been in production long enough to trust on the maker side.

## 4. Phase overview

The issue proposes three phases. We split them further so each PR is small
enough to review without a marathon session. Data-model and payout plumbing
come early (Phase 0 & 3) and are reused by every subsequent slash path.

| Phase | PR scope | Depends on | Status |
|------:|----------|------------|--------|
| 0 | Foundation: config schema, `bonds` table, pure helpers, types | — | ✅ shipped (PR #712) |
| 1 | Taker bond lifecycle: **lock + always release** (no slashing yet) | 0 | ✅ shipped |
| 2 | Taker bond: slash on **lost dispute** | 1 | pending |
| 3 | Payout flow: `add-invoice` to winner, routing-fee estimation, retries, audit event | 2 | pending |
| 4 | Taker bond: slash on **timeout** (apply_to=take, slash_on_waiting_timeout) | 3 | pending |
| 5 | Maker bond (non-range): lock + dispute slash reusing phase 3 payout | 3 | pending |
| 6 | Maker bond for **range orders** with proportional slashes | 5 | pending |
| 7 | Maker bond: slash on **timeout** | 5 | pending |
| 8 | Public config exposure (Mostro info event) + operator docs polish | 7 | pending |

Phases 4, 5, 6, 7 can partially overlap in time but must land in this order on
`main` to keep review scope honest.

---

## 5. Phase 0 — Foundation ✅ Completed

Purely additive. Touches no trade flow.

### 5.1 Scope

- `AntiAbuseBondSettings` struct in `src/config/types.rs` with a matching
  `Default` impl (`enabled = false`). Add `Option<AntiAbuseBondSettings>` field
  to `Settings` plus `Settings::get_bond()` accessor.
- Update `settings.tpl.toml` with the block from §3, fully commented so
  existing configs behave identically when merged.
- Pure function `compute_bond_amount(order_amount_sats: i64, cfg: &AntiAbuseBondSettings) -> i64`
  in `src/app/bond/math.rs` (new module). Returns
  `max((cfg.amount_pct * order_amount_sats).round() as i64, cfg.base_amount_sats)`
  with saturating arithmetic. Tests for: 0% percentage, floor dominates,
  percentage dominates, huge amount saturation.
- Enums (new file `src/app/bond/types.rs`):
  - `BondRole { Maker, Taker }`
  - `BondState { Requested, Locked, Released, PendingPayout, Slashed, Failed }`
  - `BondSlashReason { LostDispute, Timeout }`
- Database migration
  `migrations/<ts>_anti_abuse_bond.sql` creating:

  ```sql
  CREATE TABLE IF NOT EXISTS bonds (
    id               char(36) primary key not null,
    order_id         char(36) not null,
    -- Phase 6: parent maker bond a child slash row belongs to. NULL for
    -- parent / non-range rows.
    parent_bond_id   char(36),
    -- Phase 6: child (range-taken) order id this row represents. NULL for
    -- parent / non-range rows.
    child_order_id   char(36),
    pubkey           char(64) not null,    -- trade pubkey of the bonded party
    role             varchar(8) not null,  -- 'maker' | 'taker'
    amount_sats      integer not null,
    -- Phase 6: running total of sats already transitioned to a slashed
    -- child. 0 for child / non-range rows.
    slashed_share_sats integer not null default 0,
    state            varchar(16) not null, -- BondState
    slashed_reason   varchar(16),          -- BondSlashReason when state=Slashed
    hash             char(64),             -- payment hash of bond hold invoice
    preimage         char(64),             -- preimage retained by Mostro
    payment_request  text,                 -- bolt11 shown to payer
    payout_invoice   text,                 -- payout invoice from winner (Phase 3+)
    -- Phase 3: routing-fee ceiling actually used for the payout attempt
    -- (sats). NULL until the scheduler tries to pay the winner.
    payout_routing_fee_sats integer,
    payout_attempts  integer not null default 0,
    locked_at        integer,
    released_at      integer,
    slashed_at       integer,
    created_at       integer not null,
    FOREIGN KEY(order_id) REFERENCES orders(id)
  );
  CREATE INDEX IF NOT EXISTS idx_bonds_order_id ON bonds(order_id);
  CREATE INDEX IF NOT EXISTS idx_bonds_state    ON bonds(state);
  CREATE INDEX IF NOT EXISTS idx_bonds_parent   ON bonds(parent_bond_id);
  ```

  The Phase 0 migration lands the full column set up-front (parent/child
  range columns, `slashed_share_sats`, `payout_routing_fee_sats`) rather
  than staging ALTER TABLEs per phase. Later phases only add code, not
  schema.

  Run `cargo sqlx prepare -- --bin mostrod` to refresh `sqlx-data.json`.
- `Bond` model (sqlx-crud) and repository helpers in `src/app/bond/db.rs`:
  `create_bond`, `find_bond_by_order_and_role` (parent rows only —
  filters on `parent_bond_id IS NULL`), `find_bonds_by_state`,
  `update_bond`.
- Unit tests for each helper.

### 5.2 Non-goals

- No LND calls, no take flow edits, no scheduler hooks. Just the building
  blocks.

### 5.3 Acceptance

- `cargo test` green; new tests for `compute_bond_amount` and CRUD helpers.
- `cargo clippy --all-targets --all-features` clean.
- Toggling `enabled = true` does nothing yet — verified by explicit test that
  spins up the daemon config and asserts no handler branches on the bond
  config.

---

## 6. Phase 1 — Taker bond: lock + always release ✅ Completed

Wire the bond into the take flow but **never slash**. This lets operators
turn the feature on in staging to exercise hold-invoice custody with zero risk
to users.

**Implementation notes (as shipped):**

- The phase took the §6.2 "Alternative" path: orders stay in `Status::Pending`
  while the bond is outstanding, and the bond bolt11 is delivered to the
  taker via the existing `Action::PayInvoice` (the bond's payment hash
  uniquely distinguishes it from the trade hold invoice that follows). The
  dedicated `Status::WaitingTakerBond` / `Action::AddBondInvoice` will be
  introduced in the matching `mostro-core` release alongside a later
  phase, at which point this can be migrated transparently.
- Bond release is wired into every Phase 1 exit:
  `release_action`, `cancel_action` (cooperative + unilateral, taker- and
  maker-side, including pending-order maker cancels), `admin_settle_action`,
  `admin_cancel_action`, and `scheduler::job_cancel_orders`. Slashing
  hooks are intentionally absent and land in Phase 2+.
- `take_buy_action` / `take_sell_action` call
  `bond::supersede_prior_taker_bonds` before persisting the new take.
  A still-`Requested` prior bond is released (its hold invoice
  cancelled) so a malicious taker can't keep an order in `Pending`
  by abandoning the bond invoice — anyone may re-take and the first
  bond to lock wins. A `Locked` prior bond is treated as committed
  and the new take is rejected with `PendingOrderExists`.
- `cancel_action` recognises a bonded taker as authorised to cancel a
  still-`Pending` order: when `event.sender` matches the `pubkey` of an
  active bond on the order, the cancel routes through the existing
  `cancel_order_by_taker` flow (release the bond, clear the taker
  fields, republish the order). This lets a taker who took the order
  but no longer wants to proceed back out cleanly instead of getting
  `IsNotYourOrder`.
- On daemon startup, `bond::resubscribe_active_bonds` re-attaches LND
  invoice subscribers for any bond rows still in `Requested` / `Locked`,
  so a restart never strands a taker who paid the bond just before the
  daemon went down.

### 6.1 Scope

- Gate `enabled && apply_to ∈ { "take", "both" }`. Otherwise existing code
  path is untouched.
- `src/app/bond/flow.rs`:
  - `request_taker_bond(ctx, order, taker_pubkey, request_id) -> Result<Bond, MostroError>`
    - Compute bond amount from order amount (use max_amount for range orders
      so Phase 6 migration is trivial — but range-maker handling is Phase 6;
      for taker on a range order we use the exact amount chosen by the taker).
    - `LndConnector::create_hold_invoice` with a dedicated memo
      (`"mostro bond order_id=<uuid>"`).
    - Persist `Bond { state: Requested, hash, preimage, payment_request }`.
    - Enqueue a new `Action::AddBondInvoice` message to the taker with the
      bolt11 so the client pays it.
    - `LndConnector::subscribe_invoice(hash, ..)` feeding a bond listener
      (new file `src/flow.rs` companion or inside `bond/flow.rs`). On
      `Accepted`, transition the bond to `Locked`.
  - `release_bond(ctx, &Bond)` — `cancel_hold_invoice(hash)`, mark
    `Released`, set `released_at`.
- Modify `take_buy_action` / `take_sell_action`:
  - If the bond is enabled for take, **do not** call `show_hold_invoice` yet.
    Instead, stash all computed trade state on the order row (status stays
    `Pending` or a new ephemeral `WaitingTakerBond` status — see §6.2), call
    `request_taker_bond`, and return.
  - Add an internal continuation that fires when the bond subscriber reports
    `Accepted`. It resumes the original take: calls `show_hold_invoice`
    (sell-side) or `set_waiting_invoice_status` (buy-side), flipping order
    status to `WaitingPayment` / `WaitingBuyerInvoice`.
- Wire bond release into every existing exit:
  - `release_action` settle path → `release_bond` after seller invoice
    settles.
  - `cancel_action` (cooperative and unilateral) → `release_bond` whenever
    the hold escrow is canceled.
  - `admin_cancel_action`, `admin_settle_action` → always `release_bond` in
    Phase 1 (slash logic lands in Phase 2).
  - `scheduler::job_cancel_orders` → when an order is reset/canceled,
    release bonds attached to it.
- New status `WaitingTakerBond` in `mostro_core::order::Status` (requires a
  PR in `mostro-core` first; note: this doc only tracks the Mostro-daemon
  side). **Alternative**: reuse `Pending` and track bond state in the
  `bonds` table alone. Decision deferred to PR review; recommendation is
  the explicit status for observability.
- Tests:
  - Happy path: take → bond locked → escrow flow → release settles bond.
  - Taker never funds the bond hold invoice → expiration leaves order
    returned to `Pending` (no bond outstanding; no user harmed).
  - Cooperative cancel after bond locked → bond released.

### 6.2 Open question to resolve in the PR

- **New `WaitingTakerBond` status vs reusing `Pending`.**
  - Adding a status is a breaking protocol hint for clients that filter
    status tags; should coincide with a minor version bump in mostro-core.
  - Reusing `Pending` is cheaper but makes it harder for clients to render
    "waiting for your bond."
  - Default recommendation: **add the status** because it matches the new
    `AddBondInvoice` action and is forward-compatible with the maker bond
    that will need a similar state.

### 6.3 Acceptance

- Feature disabled: no behavior change. Integration test that takes an order
  with `enabled=false` passes identically.
- Feature enabled, apply_to=take: taker must fund a second hold invoice; all
  exits release it.

---

## 7. Phase 2 — Taker dispute slash

Behavior gate: `enabled && apply_to ∈ {take, both} && slash_on_lost_dispute`.

### 7.1 Scope

- `admin_settle_action` and `admin_cancel_action` already know who won (seller
  wins → settle hold; buyer wins → cancel hold and payout).
  - For each outcome, compute whether the **taker** lost.
    - Taker identity is already derivable: on a Buy order the taker is the
      seller, on a Sell order the taker is the buyer.
  - If taker lost and slash flag on → set taker bond
    `state = PendingPayout, slashed_reason = LostDispute`. Do **not** settle
    the bond hold invoice yet — that's Phase 3 so it coincides with the
    payout flow.
  - If taker won (or flag off) → `release_bond`.
- Tests for both disputes (Buy and Sell), both outcomes, flag on/off.

### 7.2 Non-goals

- No actual Lightning payout in this phase. `PendingPayout` is persisted and
  surfaces in logs; the scheduler in Phase 3 picks it up.

### 7.3 Acceptance

- With flag off: Phase 1 behavior preserved (release).
- With flag on + taker lost: bond row is `PendingPayout`.
- With flag on + taker won: bond released.

---

## 8. Phase 3 — Payout flow

Shared infrastructure used by every slash path afterwards. Non-blocking:
trade finalization must never wait on the payout.

### 8.1 Scope

- New scheduler job `job_process_bond_payouts` in `src/scheduler.rs`,
  mirroring `job_process_dev_fee_payment`:
  - Polls `PendingPayout` bonds at a fixed interval (default 60s).
  - For each:
    1. If no `payout_invoice` yet, and either no outstanding DM to the
       winner or the window has elapsed: enqueue an `Action::AddInvoice`
       DM to the winning counterparty asking for a bolt11 for
       `amount_sats - estimated_routing_fee`. Increment
       `payout_attempts`.
    2. If an invoice was received (see handler below), **estimate the
       routing fee** via `LndConnector::query_routes(dest, amount)`
       (thin wrapper over LND `router::query_routes`); fall back to
       `amount * max_routing_fee` if the RPC fails.
    3. `settle_hold_invoice(preimage)` on the bond hash to claim the
       forfeited sats into Mostro's wallet.
    4. `send_payment` to the winner invoice with capped fee.
    5. On success → `state = Slashed`, `slashed_at = now`, publish audit
       event.
    6. On failure → bump `payout_attempts`; once `payout_max_retries`
       reached, transition to `Failed` and leave a tracing error.
- New action handler `add_bond_invoice_action` in a new
  `src/app/bond/payout.rs` module. Receives an `Action::AddInvoice` reply
  from a bond-payout candidate (distinct from the buyer-invoice
  `add_invoice_action` — disambiguated by the presence of a bond row in
  `PendingPayout` for the order / sender pair).
- Audit event: new Nostr kind (reusing the dev-fee style: custom kind
  number, registered in `src/config/constants.rs`). Tags:
  `order-id`, `role`, `reason`, `amount`, `hash` (bond payout hash,
  **not** the trade payment hash), `y`, `z = bond-slash`. No counterparty
  pubkeys, consistent with dev-fee audit privacy. Expiration uses
  `ExpirationSettings` with a new optional `bond_slash_days` field.
- Unit tests: routing-fee fallback, retries exhaustion, settle-then-pay
  ordering.

### 8.2 Failure modes & invariants

- **`settle` must succeed before `send_payment`.** If settle fails we leave
  the bond in `PendingPayout` and retry on the next tick; the bonded party's
  HTLC stays held, which is the correct safety posture.
- **Partial success: settle OK, send_payment failed.** The bond state moves
  to `PendingPayout` with a best-effort retry. The winner is kept informed
  via periodic DMs. If retries exhaust, state becomes `Failed`; at that
  point Mostro keeps the sats (unavoidable with the HTLC settled) and logs
  loudly. This is a known limitation to be addressed only if it shows up
  in practice; node operators can manually pay the winner from logs.
- **Non-blocking:** `release_action`, `admin_settle_action`, etc. return
  success the moment the trade escrow resolves. Bond payout happens later.

### 8.3 Acceptance

- End-to-end test: dispute-lost taker bond → winner gets a DM → winner
  submits bolt11 → bond payout settles → Nostr audit event published.
- Retry test: winner never answers → scheduler keeps retrying up to
  `payout_max_retries`, then `Failed`.

---

## 9. Phase 4 — Taker timeout slash

Gate: `enabled && apply_to ∈ {take, both} && slash_on_waiting_timeout`.

### 9.1 Scope

- Critical invariant (from the issue): **bond is slashed only when the
  timeout actually elapsed.** Cancels before the timeout — mutual,
  unilateral, admin — must always release the bond. This prevents a
  malicious maker from cancelling at minute N-1 to steal a taker's bond.
- Modify `scheduler::job_cancel_orders`:
  - Today it walks orders whose `taken_at` + timeout ≤ now. That path is
    the one — and only one — trigger for a timeout slash. Cancels from
    `cancel_action` / `admin_cancel_action` do not enter this path, so
    the invariant holds by construction.
  - When the waiting-state timeout elapses on an order in
    `WaitingBuyerInvoice` / `WaitingPayment`:
    - Identify the *responsible* side (the one that didn't do their duty):
      - `WaitingBuyerInvoice` → buyer is responsible.
      - `WaitingPayment` → seller is responsible.
    - Is that party the **taker**? (Sell order + waiting-payment = taker
      is seller; Buy order + waiting-buyer-invoice = taker is buyer; etc.)
    - If yes and the slash flag is on: set taker bond
      `state = PendingPayout, slashed_reason = Timeout`. Then continue
      the existing behavior (cancel escrow hold invoice, republish
      order as `Pending` for maker-initiated flows). Order republishes
      — the maker doesn't need to re-post.
- Message to the slashed user: new localized string explaining forfeiture
  with the configured flag value.
- Tests:
  - "Cancel at minute 5 of a 15-minute timeout" — bond released, no slash.
  - "Taker silent past 15-minute timeout" — bond slashed,
    `BondSlashReason::Timeout`, order returned to `Pending`.
  - Flag off → no slash even when timeout elapses (old behavior).

### 9.2 Acceptance

- Attack-invariant test passes: malicious maker cancels before timeout →
  bond always released, even across both flag values.
- Timeout slash feeds Phase 3 payout flow.

---

## 10. Phase 5 — Maker bond (non-range) + dispute slash

Gate: `enabled && apply_to ∈ {create, both}`.

### 10.1 Scope

- Extend `publish_order` (`src/util.rs`) to request a bond from the maker
  when enabled, **before** the order is published to Nostr. The order row
  exists in DB with a transient status (recommend `WaitingMakerBond` in
  `mostro-core`). No NIP-33 order event is emitted until the bond is
  `Locked`.
- Once the bond subscriber reports `Accepted`, continue the existing
  `publish_order` work (compute tags, emit event, set `event_id`).
- Bond release hooks:
  - Order completed → `release_bond`.
  - Order cancelled before ever being taken → `release_bond`.
  - Order expired in `Pending` → `release_bond`.
  - Maker loses dispute and `slash_on_lost_dispute` → `PendingPayout`
    with reason `LostDispute`, reusing Phase 3 payout.
  - Maker wins dispute → `release_bond`.
- Tests for each.

### 10.2 Open question: bond amount for a range maker order

A range order has `min_amount`/`max_amount` in fiat but no single sats
amount up front. **Use `max_amount` converted at current price** to size
the maker bond — consistent with "worst-case exposure." That's also how
Phase 6 will split proportional slashes. If the price drifts between
publication and take, the bond is computed against the sats value at
publication time and is not repriced.

### 10.3 Acceptance

- Feature disabled: no change.
- Feature enabled, apply_to=create: order is not visible in the book until
  bond locks. A client that abandons the bond invoice → order never shows
  up; no ghost book entry.

---

## 11. Phase 6 — Range-order maker bond with proportional slashes

Dependent on Phase 5. This is the only genuinely subtle phase; keep the
review bar high.

### 11.1 Data model addition

Extend `Bond` with:
- `child_order_id char(36)` — set on a **child bond row** that represents a
  partial slash/release on the parent.
- `parent_bond_id char(36)` — on child rows, references the maker's parent
  bond.
- `slashed_share_sats integer default 0` — on the parent bond row,
  running total of sats already slashed.

The maker posts **one** hold invoice (parent) sized against `max_amount`.
When a child order is created inside the range, no new maker bond hold
invoice is needed — we track the slice via a child row only if/when we
need to slash.

### 11.2 Slash math

For a child order with sats amount `child_sats` and parent bond computed
from `parent_max_sats`:

```text
share_fraction  = child_sats / parent_max_sats
slash_amount    = round(parent_bond_amount * share_fraction)
```

On dispute loss for that child (and slash flag on):
- Insert a child bond row (`parent_bond_id`, `amount_sats = slash_amount`,
  `state = PendingPayout`, `slashed_reason = LostDispute`).
- The scheduler's payout job queries the winner, receives an invoice for
  `slash_amount - routing_fee`, claims `slash_amount` from the parent hold
  invoice… but LND hold invoices do not natively support partial claims.

**Implementation reality for partial slash:**
A BOLT11 hold invoice is all-or-nothing. We cannot settle only part of it.
The workable strategies:

1. **Accumulate and settle-at-close.** Track `slashed_share_sats` on the
   parent. Keep the parent hold invoice locked for the entire life of the
   range order. At parent close (exhaustion / expiration / cancellation):
   - If `slashed_share_sats == 0` → `cancel_hold_invoice(parent)` and
     release.
   - If `slashed_share_sats == parent_bond_amount` → `settle` and payout
     the accumulated winnings (multiple winners are supported by keeping
     child rows with their own `payout_invoice`).
   - If partial → there is no way to claim exactly the slashed sats from a
     single HTLC; we must choose between:
     - **(a)** Claim the whole bond, pay out the slashed share to winners,
       and refund the unslashed share back to the maker via `add-invoice`
       (maker becomes a counterparty here). Reuses Phase 3 plumbing.
     - **(b)** Never lock a parent bond; require a per-child bond at take
       time. Simpler to reason about but breaks the issue's requirement
       that the maker bond exists **before** the order is visible.

We recommend **(a)**. Acceptable cost: maker sees one extra `add-invoice`
DM at range-close if there were partial slashes.

2. **Fallback on HTLC expiry.** If the range order is still active when
   the hold invoice CLTV is about to expire, the scheduler must settle or
   cancel before LND does it for us. `hold_invoice_cltv_delta` in settings
   bounds this. Document the operator impact.

### 11.3 Scope

- Parent/child bond rows and helpers.
- Range-order publication sizes bond against `max_amount`.
- Child slash: creates child row in `PendingPayout`; does not touch the
  parent HTLC yet.
- Parent close: resolve according to strategy (a) above.
- Extensive tests:
  - No slashes → full release to maker on close.
  - One small child slashed → pay winner, refund unslashed to maker.
  - Multiple child slashes across the range → each winner paid, residual
    refunded.
  - Cancellation during active children → pending slashes still
    actionable after.

### 11.4 Acceptance

- Issue invariant from §"Range Orders" satisfied: proportional slash, full
  release on close, child independence.
- No path exists that settles the parent hold invoice before the range is
  resolved.

---

## 12. Phase 7 — Maker timeout slash

Gate: `enabled && apply_to ∈ {create, both} && slash_on_waiting_timeout`.

Symmetric to Phase 4. The responsible party on a given waiting state might
be the maker (e.g. a sell-maker who never paid the hold invoice after the
buyer-taker submitted their invoice). Reuse the dispatch in
`job_cancel_orders` and reuse the Phase 3 payout.

Keep the Phase 4 invariant: cancels before timeout always release.

Tests mirror Phase 4 from the maker side. For range orders, a per-child
timeout slashes the child's share via the Phase 6 partial-slash path.

---

## 13. Phase 8 — Public exposure + docs

### 13.1 Scope

- Extend the Mostro info event (`src/nip33.rs::info_to_tags`) with the
  bond config snapshot so clients can show users what the node enforces
  before they trade:
  - `bond` (`enabled` | `disabled`)
  - `bond-apply-to` (`take`/`create`/`both`)
  - `bond-slash-dispute` (`true`/`false`)
  - `bond-slash-timeout` (`true`/`false`)
  - `bond-amount-pct` / `bond-amount-floor`
- README + `docs/ARCHITECTURE.md`: add the bond flow to the per-action
  table and to the sequence diagrams.
- `docs/LIGHTNING_OPS.md`: operator runbook section for bonds (how to
  read audit events, how to resolve a `Failed` bond manually).
- `CHANGELOG.md` entry (user-visible: new optional config, opt-in).

### 13.2 Acceptance

- `mostro info` output (and client-visible info tags) surfaces the bond
  policy.
- Docs build; markdown lint clean.

---

## 14. Cross-cutting concerns

### 14.1 Privacy

- The bond audit event deliberately omits buyer/seller pubkeys (dev-fee
  precedent). The only party-identifying info that could leak is the
  payout invoice's routing hints, which is unavoidable but does not
  reveal more than a normal Lightning payment.

### 14.2 Backward compatibility

- Database migration is purely additive; existing orders are unaffected.
- Default config is `enabled = false`; old `settings.toml` files work as
  is.
- New `Status` / `Action` variants in `mostro-core` (Phases 1 & 5) must
  ship in that crate first and be pinned to a version in this repo's
  `Cargo.toml`. Clients must handle unknown statuses gracefully — this
  is already the case.

### 14.3 Protocol/tag changes

Per `CONTRIBUTING.md § Protocol / Tag Changes`, the new info-event tags
(Phase 8) require a compatibility statement in the PR body. New
`AddBondInvoice`, `BondLocked`, `BondSlashed` actions (Phases 1–4)
similarly need a cross-kind scope declaration.

### 14.4 Testing discipline

- Unit tests per phase as listed above, co-located under
  `#[cfg(test)] mod tests` in the touched module.
- Integration-style tests against an in-memory SQLite (see existing
  patterns in `src/util.rs::tests`).
- Manual LND regression checklist in each PR body:
  - lock a bond on polar/regtest
  - release via normal flow
  - release via cancel
  - (from Phase 2) slash via dispute
  - (from Phase 3) winner receives payout

### 14.5 Observability

- `tracing` spans in each bond transition with
  `bond_id`, `order_id`, `role`, `state`.
- A structured log line on every state change is enough; no Prometheus
  wiring needed until we see real traffic.

---

## 15. Answers to the issue's open questions

1. **Hold invoice library support.** LND already powers hold invoices in
   Mostro (`LndConnector::create_hold_invoice`). The bond reuses this
   primitive; no new library.
2. **Preimage custody.** Already held by Mostro today
   (`orders.preimage`). Bonds follow the same model in the new
   `bonds.preimage` column.
3. **Order-book visibility with maker bond.** Phase 5 defers publishing
   the NIP-33 order event until the bond locks. Confirmed approach.
4. **Rollover on re-take/re-create.** Each `take` or `create` is
   independent; a fresh bond row is created each time. No rollover.
5. **Dispute state machine.** Today's resolution flows
   (`admin_settle_action`, `admin_cancel_action`) already know the
   winning side; Phase 2 leverages that directly.
6. **Routing-fee estimation.** LND `router::query_routes` gives a pre-
   payment fee estimate. Fallback: `amount * max_routing_fee` using the
   existing Mostro setting. Implemented in Phase 3.
7. **Range-order partial slash tracking.** Phase 6 introduces
   parent/child bond rows and accumulates `slashed_share_sats` on the
   parent, resolving the HTLC at range close. Strategy (a) detailed in
   §11.2.

---

## 16. Tracking

Each phase ships as a separate PR that links this document. The PR
description must state: which phase, which gate flags it touches, and
the manual LND/regtest evidence that the bond behaved correctly.

When the full plan has landed, this spec is kept in `docs/` as the
feature's reference.
