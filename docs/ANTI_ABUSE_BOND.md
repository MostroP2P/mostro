# Anti-Abuse Bond — Implementation Spec

> Implementation guide for [issue #711](https://github.com/MostroP2P/mostro/issues/711).
> This document is the single source of truth as the feature is rolled out across
> several PRs. Each phase below maps to one **small, atomic PR** that can be
> reviewed, tested, and released independently.

## 1. Goal

Add an **opt-in, node-level** anti-abuse bond. A user (taker and/or maker)
locks a Lightning hold invoice when entering a trade. The bond is:

- **Released** on normal completion, on any cancellation that happens before
  a waiting-state timeout elapses, and on dispute resolutions where the
  solver does not direct otherwise.
- **Slashed** (claimed by Mostro and paid to the counterparty) under two
  unambiguous conditions:
  1. **Solver directive on dispute resolution.** The solver explicitly
     instructs Mostro to slash one or both bonds via the `BondResolution`
     payload of `admin-settle` / `admin-cancel`. The slash decision is
     independent from the trade outcome (settle vs cancel). See §15 for
     the worked example that motivates the decoupling.
  2. **Waiting-state timeout** elapsed (scheduler path), gated per-node by
     `slash_on_waiting_timeout`.

Disabled by default. Nodes that don't flip `enabled = true` see zero behavior
change.

## 2. Guiding principles

1. **Opt-in and off-by-default.** Every phase must preserve today's behavior
   when `[anti_abuse_bond].enabled = false`.
2. **Atomic PRs.** Each phase below is scoped to one coherent change that can
   ship (and revert) independently. No phase leaves the daemon in an
   incoherent state.
3. **Slash is decoupled from trade outcome.** A slash decision is never
   derived automatically from settle/cancel. It is either an explicit solver
   directive (for disputes) or a deterministic single-party timeout (for the
   scheduler path). When the solver does not direct a slash, bonds are
   released. See §15.
4. **Buyer/seller for slash logic; maker/taker for posting timing.** The
   bond is requested at the maker's create-time or the taker's take-time
   (`apply_to`), but the actions whose failure justifies a slash are
   buyer/seller actions (paying the hold invoice, providing a buyer
   invoice, sending fiat, releasing). See §3.1.
5. **Never slash unless the cause is unambiguous.** The worst failure mode
   is stealing a good user's bond. When in doubt, release.
6. **Bond is separate from escrow.** A second hold invoice, its own row, its
   own lifecycle. It must not be deducted from or conflated with trade
   escrow.
7. **Tests accompany every phase.** Rust unit tests co-located with the
   module; `cargo test`, `cargo clippy --all-targets --all-features`, and
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

# Which flow posts a bond. This is intentionally a maker/taker switch
# because it is a *posting-timing* distinction — the bond is requested at
# maker-create-time or taker-take-time. The party's role in the trade
# flow (buyer or seller) is derived from the order kind (§3.1).
#   "take"   → only the taker posts a bond
#   "create" → only the maker posts a bond
#   "both"   → both sides
apply_to = "take"

# Automatic slash on waiting-state timeout. Only applies when the
# scheduler-driven timeout actually elapses; user-initiated and admin
# cancels never trigger this path.
slash_on_waiting_timeout = false

# Bond payout retries (applies once a bond is slashed and Mostro needs
# a payout invoice from the winning counterparty).
payout_invoice_window_seconds = 300
payout_max_retries           = 5
```

Note: there is **no `slash_on_lost_dispute` flag**. Dispute slashes are
expressed by the solver per-resolution via the `BondResolution` payload
(see Phase 2). A node that does not want to use slashes simply has its
solver never emit non-zero `BondResolution` payloads.

`apply_to` is deliberately expressive so phases can be rolled out one side
at a time on production nodes — operators can keep `apply_to = "take"`
until Phase 5 has been in production long enough to trust on the maker
side.

### 3.1 Two axes the spec is careful to keep separate

This feature touches two distinct axes that must not be conflated:

- **Maker / taker — *who posted the bond, and when.*** The bond is
  requested at order-creation time (maker) or at take time (taker).
  `apply_to` is a maker/taker switch. `BondRole` in the data model is a
  maker/taker enum. Phase 1's "supersede on retake" and Phase 5's
  "publish-after-bond-locks" gating are genuinely maker/taker concerns
  because they are about *order-flow* actions that only one role can
  perform.
- **Buyer / seller — *whose action triggers a slash.*** All trade-flow
  duties (paying the hold invoice, providing the buyer invoice, sending
  fiat, releasing) are buyer/seller duties. Timeout responsibility maps
  cleanly: `WaitingBuyerInvoice → buyer`, `WaitingPayment → seller`. The
  `BondResolution` payload that solvers send carries `slash_seller` /
  `slash_buyer`, never `slash_maker` / `slash_taker`.

The mapping between the two axes is fixed by the order kind:

| Order kind | maker is | taker is |
|------------|----------|----------|
| `sell`     | seller   | buyer    |
| `buy`      | buyer    | seller   |

So a `slash_seller` directive on a sell-order resolves to the maker's bond
row; on a buy-order it resolves to the taker's bond row. The daemon does
this resolution internally — solvers and clients only think in
buyer/seller terms.

## 4. Phase overview

The issue proposes three phases. We split them further so each PR is small
enough to review without a marathon session. Data-model and payout
plumbing come early (Phase 0 & 3) and are reused by every subsequent
slash path.

| Phase | PR scope | Depends on | Status |
|------:|----------|------------|--------|
| 0 | Foundation: config schema, `bonds` table, pure helpers, types | — | ✅ shipped (PR #712) |
| 1 | Taker bond lifecycle: **lock + always release** (no slashing yet) | 0 | ✅ shipped (PR #719) |
| 2 | Solver-directed dispute slash via `BondResolution` payload (taker bond) | 1 | pending |
| 3 | Payout flow: `add-invoice` to winner, routing-fee estimation, retries, audit event | 2 | pending |
| 4 | Timeout slash for taker bond (`slash_on_waiting_timeout`) | 3 | pending |
| 5 | Maker bond (non-range): lock + dispute slash reusing Phase 2/3 | 3 | pending |
| 6 | Maker bond for **range orders** with proportional slashes | 5 | pending |
| 7 | Timeout slash for maker bond | 5 | pending |
| 8 | Public config exposure (Mostro info event) + operator docs polish | 7 | pending |

Phases 4, 5, 6, 7 can partially overlap in time but must land in this
order on `main` to keep review scope honest.

---

## 5. Phase 0 — Foundation ✅ Completed

Purely additive. Touches no trade flow.

### 5.1 Scope

- `AntiAbuseBondSettings` struct in `src/config/types.rs` with a matching
  `Default` impl (`enabled = false`). Add `Option<AntiAbuseBondSettings>`
  field to `Settings` plus `Settings::get_bond()` accessor.
- Update `settings.tpl.toml` with the block from §3, fully commented so
  existing configs behave identically when merged.
- Pure function `compute_bond_amount(order_amount_sats: i64, cfg: &AntiAbuseBondSettings) -> i64`
  in `src/app/bond/math.rs` (new module). Returns
  `max((cfg.amount_pct * order_amount_sats).round() as i64, cfg.base_amount_sats)`
  with saturating arithmetic. Tests for: 0% percentage, floor dominates,
  percentage dominates, huge amount saturation.
- Enums (new file `src/app/bond/types.rs`):
  - `BondRole { Maker, Taker }` — *posting-timing* role; see §3.1.
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
- Toggling `enabled = true` does nothing yet — verified by explicit test
  that spins up the daemon config and asserts no handler branches on the
  bond config.

---

## 6. Phase 1 — Taker bond: lock + always release ✅ Completed

Wire the bond into the take flow but **never slash**. This lets operators
turn the feature on in staging to exercise hold-invoice custody with zero
risk to users.

**Implementation notes (as shipped):**

- Orders stay in `Status::Pending` while the bond is outstanding, and the
  bond bolt11 is delivered to the taker via the existing
  `Action::PayInvoice` (the bond's payment hash uniquely distinguishes it
  from the trade hold invoice that follows). A dedicated
  `Status::WaitingTakerBond` / `Action::AddBondInvoice` will be introduced
  in the matching `mostro-core` release alongside a later phase, at which
  point this can be migrated transparently.
- Bond release is wired into every Phase 1 exit:
  `release_action`, `cancel_action` (cooperative + unilateral, taker- and
  maker-side, including pending-order maker cancels), `admin_settle_action`,
  `admin_cancel_action`, and `scheduler::job_cancel_orders`. Slashing
  hooks are intentionally absent and land in Phase 2+.
- `take_buy_action` / `take_sell_action` call
  `bond::supersede_prior_taker_bonds` before persisting the new take. A
  still-`Requested` prior bond is released (its hold invoice cancelled)
  so a malicious taker can't keep an order in `Pending` by abandoning the
  bond invoice — anyone may re-take and the first bond to lock wins. A
  `Locked` prior bond is treated as committed and the new take is
  rejected with `PendingOrderExists`.
- `cancel_action` recognises a bonded taker as authorised to cancel a
  still-`Pending` order: when `event.sender` matches the `pubkey` of an
  active bond on the order, the cancel routes through the existing
  `cancel_order_by_taker` flow (release the bond, clear the taker
  fields, republish the order). This lets a taker who took the order but
  no longer wants to proceed back out cleanly instead of getting
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
    - Compute bond amount from order amount (use the exact amount chosen
      by the taker for range orders).
    - `LndConnector::create_hold_invoice` with a dedicated memo
      (`"mostro bond order_id=<uuid>"`).
    - Persist `Bond { state: Requested, hash, preimage, payment_request }`.
    - Enqueue an `Action::PayInvoice` message to the taker with the
      bolt11 so the client pays it.
    - `LndConnector::subscribe_invoice(hash, ..)` feeding a bond listener.
      On `Accepted`, transition the bond to `Locked` and resume the take.
  - `release_bond(ctx, &Bond)` — `cancel_hold_invoice(hash)`, mark
    `Released`, set `released_at`.
- Wire bond release into every existing exit (see notes above).
- Tests:
  - Happy path: take → bond locked → escrow flow → release settles bond.
  - Taker never funds the bond hold invoice → expiration leaves order
    returned to `Pending` (no bond outstanding; no user harmed).
  - Cooperative cancel after bond locked → bond released.

### 6.2 Acceptance

- Feature disabled: no behavior change. Integration test that takes an
  order with `enabled=false` passes identically.
- Feature enabled, apply_to=take: taker must fund a second hold invoice;
  all exits release it.

---

## 7. Phase 2 — Solver-directed dispute slash

Behaviour gate: `enabled && apply_to ∈ { take, both }` (Phase 5 extends to
maker).

This phase introduces the protocol mechanism by which a solver, while
resolving a dispute, instructs Mostro on **two independent decisions**
carried by the same admin message:

1. Where the trade escrow goes — `admin-settle` (sats to buyer) or
   `admin-cancel` (sats to seller). Unchanged from today.
2. Which bonds to slash, if any — new `BondResolution` payload.

Earlier drafts of this spec coupled these two decisions ("the loser of
the dispute is the loser of the bond") and the resulting ambiguities are
catalogued in §15. The decoupled model is what this phase ships.

### 7.1 New `BondResolution` payload variant in `mostro-core`

In `mostro-core::message::Payload`:

```rust
/// Bond resolution carried by [`Action::AdminSettle`] / [`Action::AdminCancel`].
/// Lets the solver express slash decisions independently of the trade
/// outcome (settle vs cancel). Absent payload (`null`) ⇒ neither bond
/// is slashed (release-by-default; honours the "when in doubt, release"
/// invariant in §2.5).
BondResolution(BondResolution)
```

```rust
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BondResolution {
    /// Slash the seller's bond if posted.
    pub slash_seller: bool,
    /// Slash the buyer's bond if posted.
    pub slash_buyer: bool,
}
```

`MessageKind::verify` accepts this variant only on `Action::AdminSettle`
and `Action::AdminCancel`. Any other action carrying it returns
`ServiceError::InvalidPayload`.

This is a new payload variant, not a breaking change to the wire format
(serde additive). It does require a minor version bump of `mostro-core`
and a pinned dependency in this repo's `Cargo.toml`. See §14.3.

### 7.2 Wire format examples

Cancel + slash only the seller (the Alice scenario from §15.1):

```json
[
  {
    "order": {
      "version": 1,
      "id": "<order-id>",
      "action": "admin-cancel",
      "payload": {
        "bond_resolution": { "slash_seller": true, "slash_buyer": false }
      }
    }
  },
  null
]
```

Settle + slash only the buyer (typical: buyer falsely claimed fiat sent):

```json
{ "order": { "version": 1, "id": "<order-id>", "action": "admin-settle",
  "payload": { "bond_resolution": { "slash_seller": false, "slash_buyer": true } } } }
```

Cancel + slash both (e.g. both parties went silent):

```json
{ "order": { "version": 1, "id": "<order-id>", "action": "admin-cancel",
  "payload": { "bond_resolution": { "slash_seller": true, "slash_buyer": true } } } }
```

Settle/cancel without slash (ambiguous evidence, honest misunderstanding,
or a legacy admin client):

```json
{ "order": { "version": 1, "id": "<order-id>", "action": "admin-cancel", "payload": null } }
```

### 7.3 Daemon behaviour

`admin_settle_action` and `admin_cancel_action`:

1. Parse the payload. Absent / `null` ≡
   `BondResolution { slash_seller: false, slash_buyer: false }`.
2. Resolve `slash_seller` to the bond row of whichever party (maker or
   taker) is the seller for this order, using the §3.1 mapping. Same
   for `slash_buyer`.
3. **Validate before doing anything destructive.** If a slash is
   requested for a side whose party has no active (`Locked`) bond row,
   abort with `Action::CantDo(CantDoReason::InvalidPayload)`. The trade
   resolution itself does not run on a rejected payload — the solver is
   expected to fix the directive and resend. This makes the
   misconfiguration visible (e.g. `slash_seller=true` on a sell-order
   with `apply_to=take` cannot succeed because the seller is the maker
   and has no bond) so the operator can decide whether to widen
   `apply_to`.
4. On a valid payload: perform the trade resolution (settle or cancel)
   first, then for each bond marked for slash transition
   `state = PendingPayout, slashed_reason = LostDispute`. Bonds not
   marked for slash are cancelled (`cancel_hold_invoice`) and marked
   `Released` immediately.

The actual Lightning payout to the counterparty is asynchronous and
handled by Phase 3.

### 7.4 Validation rules summary

- `BondResolution` on any action other than `AdminSettle` / `AdminCancel`
  → `InvalidPayload` (rejected by `MessageKind::verify`).
- `BondResolution` from a non-admin sender → existing admin-only check
  rejects with `InvalidPubkey`.
- `slash_*=true` for a party with no `Locked` bond row → `InvalidPayload`
  (§7.3 step 3). This also covers the "feature disabled / no bond
  posted" case naturally.

### 7.5 Tests

- Settle + `slash_buyer=true`, taker is buyer, taker bond `Locked` → bond
  enters `PendingPayout`; trade settles.
- Cancel + `slash_seller=true` on a sell-order with `apply_to=take`
  (seller is maker, no bond) → `CantDo(InvalidPayload)`, trade does
  not cancel.
- Settle/cancel with `payload: null` → no slash, both bonds (if any)
  released; trade resolves normally.
- Both flags true with both bonds present (Phase 5 onward) → both rows
  in `PendingPayout`.
- Non-admin sending `BondResolution` → rejected before processing.

### 7.6 Acceptance

- A solver can settle or cancel a dispute and choose to slash neither,
  one, or both bonds — orthogonal decisions.
- Phase 1 behaviour is preserved when the solver omits the payload.
- The "Alice scenario" (§15.1) is expressible end-to-end.

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
       winning counterparty or the window has elapsed: enqueue an
       `Action::AddInvoice` DM to the counterparty (the buyer when
       `slash_seller`, the seller when `slash_buyer`) asking for a
       bolt11 for `amount_sats - estimated_routing_fee`. Increment
       `payout_attempts`.
    2. If an invoice was received (see handler below), **estimate the
       routing fee** via `LndConnector::query_routes(dest, amount)`
       (thin wrapper over LND `router::query_routes`); fall back to
       `amount * max_routing_fee` if the RPC fails.
    3. `settle_hold_invoice(preimage)` on the bond hash to claim the
       forfeited sats into Mostro's wallet.
    4. `send_payment` to the counterparty invoice with capped fee.
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
  `order-id`, `role` (maker/taker — which posted the bond),
  `reason` (`lost-dispute` | `timeout`),
  `amount`, `hash` (bond payout hash, **not** the trade payment hash),
  `y`, `z = bond-slash`. No counterparty pubkeys, consistent with
  dev-fee audit privacy. Expiration uses `ExpirationSettings` with a new
  optional `bond_slash_days` field.
- Unit tests: routing-fee fallback, retries exhaustion, settle-then-pay
  ordering.

### 8.2 Failure modes & invariants

- **`settle` must succeed before `send_payment`.** If settle fails we
  leave the bond in `PendingPayout` and retry on the next tick; the
  bonded party's HTLC stays held, which is the correct safety posture.
- **Partial success: settle OK, send_payment failed.** The bond state
  moves to `PendingPayout` with a best-effort retry. The winner is kept
  informed via periodic DMs. If retries exhaust, state becomes `Failed`;
  at that point Mostro keeps the sats (unavoidable with the HTLC
  settled) and logs loudly. This is a known limitation to be addressed
  only if it shows up in practice; node operators can manually pay the
  winner from logs.
- **Non-blocking:** `release_action`, `admin_settle_action`, etc. return
  success the moment the trade escrow resolves. Bond payout happens
  later.

### 8.3 Acceptance

- End-to-end test: dispute resolved with `slash_buyer=true` → buyer-side
  counterparty (the seller) gets a DM → submits bolt11 → bond payout
  settles → Nostr audit event published.
- Retry test: counterparty never answers → scheduler keeps retrying up
  to `payout_max_retries`, then `Failed`.

---

## 9. Phase 4 — Timeout slash (taker bond)

Gate: `enabled && slash_on_waiting_timeout && apply_to ∈ { take, both }`.

### 9.1 Critical invariant

**Bond is slashed only when a waiting-state timeout actually elapses.**
Cancels before the timeout — cooperative, unilateral, admin — never
slash via this path. (Admin cancels can still slash via Phase 2's
`BondResolution`, which is solver-directed and therefore not a stealth
trigger.) This prevents a malicious counterparty from cancelling at
minute N-1 to steal a bond.

### 9.2 Buyer/seller responsibility table

The scheduler's `job_cancel_orders` walks orders past their
waiting-state deadline. The responsible party is determined by the
waiting state alone:

| Waiting state          | Responsible party |
|------------------------|-------------------|
| `WaitingBuyerInvoice`  | buyer             |
| `WaitingPayment`       | seller            |

The scheduler then asks: *does that party have an active bond row?* This
is where `apply_to` and the order kind cross-check (§3.1 table). Only
when a bond exists is it transitioned to
`PendingPayout, slashed_reason = Timeout`. Otherwise the existing
cancel-and-republish behaviour runs unchanged.

Worked rows for `apply_to = "take"`:

| Order kind | Waiting state          | Responsible | Has bond? | Outcome |
|------------|------------------------|-------------|-----------|---------|
| `sell`     | `WaitingBuyerInvoice`  | buyer = taker  | yes    | slash taker bond |
| `sell`     | `WaitingPayment`       | seller = maker | no     | no slash |
| `buy`      | `WaitingBuyerInvoice`  | buyer = maker  | no     | no slash |
| `buy`      | `WaitingPayment`       | seller = taker | yes    | slash taker bond |

Phase 7 fills the "no slash" rows for `apply_to ∈ { create, both }` by
adding maker bond rows to the lookup.

### 9.3 Scope

- Modify `scheduler::job_cancel_orders`: when the waiting-state timeout
  elapses on an order in `WaitingBuyerInvoice` / `WaitingPayment`, run
  the §9.2 lookup. If a bond exists for the responsible party, set
  `state = PendingPayout, slashed_reason = Timeout` (Phase 3 picks it
  up). Continue the existing cancel-escrow + republish work.
- Localised message to the slashed user explaining forfeiture.
- Tests:
  - "Cancel at minute 5 of a 15-minute timeout" → bond released, no
    slash.
  - "Buyer silent past `WaitingBuyerInvoice`, taker = buyer" → bond
    slashed with `Timeout`, order returned to `Pending`.
  - "Seller silent past `WaitingPayment`, taker = seller" → bond
    slashed.
  - Same scenarios where the responsible party is the maker (under
    `apply_to = "take"`) → no slash, old behaviour.
  - `slash_on_waiting_timeout = false` → no slash even when timeout
    elapses.

### 9.4 Acceptance

- Attack-invariant test passes: counterparty cancelling before timeout
  never causes a slash.
- Timeout slashes feed Phase 3 payout flow.

---

## 10. Phase 5 — Maker bond (non-range) + dispute slash

Gate: `enabled && apply_to ∈ { create, both }`.

### 10.1 Bond lifecycle (maker-specific)

Bond posting and order-publication gating are *order-flow* concerns and
genuinely maker-specific (only the maker creates and publishes orders),
so the lifecycle scope is described in maker/taker terms:

- Extend `publish_order` (`src/util.rs`) to request a bond from the maker
  when enabled, **before** the order is published to Nostr. The order
  row exists in DB with a transient status (`WaitingMakerBond` in
  `mostro-core`). No NIP-33 order event is emitted until the bond is
  `Locked`.
- Once the bond subscriber reports `Accepted`, continue the existing
  `publish_order` work (compute tags, emit event, set `event_id`).

### 10.2 Slash hooks (buyer/seller via the unified mechanism)

The slash mechanics are inherited from Phase 2 (dispute) and Phase 4
(timeout) unchanged. The maker bond is just another bond row that the
buyer/seller resolution operates over:

- Order completed (release path) → maker bond released.
- Order cancelled before take, or expires `Pending` → maker bond
  released.
- Solver dispute resolution: `BondResolution { slash_seller, slash_buyer }`
  resolves to the maker bond when the maker is on the named side per
  §3.1 (sell-order → `slash_seller` targets maker; buy-order →
  `slash_buyer` targets maker). With both bonds posted, the solver can
  slash either, both, or neither, orthogonally to settle/cancel.
- Timeout slash: §9.2 table now finds the maker bond when the
  responsible party is the maker.

### 10.3 Bond amount for a range maker order

A range order has `min_amount`/`max_amount` in fiat but no single sats
amount up front. **Use `max_amount` converted at current price** to size
the maker bond — consistent with "worst-case exposure." That's also how
Phase 6 splits proportional slashes. If the price drifts between
publication and take, the bond is computed against the sats value at
publication time and is not repriced.

### 10.4 Acceptance

- Feature disabled: no change.
- Feature enabled, apply_to=create: order is not visible in the book
  until bond locks. A client that abandons the bond invoice → order
  never shows up; no ghost book entry.
- Phase 2 dispute slashes targeting the maker (e.g. `slash_seller=true`
  on a sell-order, `slash_buyer=true` on a buy-order) work end-to-end.

---

## 11. Phase 6 — Range-order maker bond with proportional slashes

Dependent on Phase 5. This is the only genuinely subtle phase; keep the
review bar high.

### 11.1 Data model

Phase 0 already shipped the columns. The maker posts **one** hold
invoice (parent) sized against `max_amount`. When a child order is
created inside the range, no new maker bond hold invoice is needed — we
track the slice via a child row only if/when we need to slash.

### 11.2 Slash math

For a child order with sats amount `child_sats` and parent bond computed
from `parent_max_sats`:

```text
share_fraction  = child_sats / parent_max_sats
slash_amount    = round(parent_bond_amount * share_fraction)
```

When a Phase 2 `BondResolution` slashes the maker on a child order (or
Phase 7 timeout does the same):
- Insert a child bond row (`parent_bond_id`, `amount_sats = slash_amount`,
  `state = PendingPayout`, `slashed_reason = LostDispute | Timeout`).
- The Phase 3 scheduler queries the counterparty for an invoice for
  `slash_amount - routing_fee`. But LND hold invoices do not natively
  support partial claims.

**Implementation reality for partial slash:**
A BOLT11 hold invoice is all-or-nothing. We cannot settle only part of
it. The workable strategies:

1. **Accumulate and settle-at-close.** Track `slashed_share_sats` on the
   parent. Keep the parent hold invoice locked for the entire life of
   the range order. At parent close (exhaustion / expiration /
   cancellation):
   - If `slashed_share_sats == 0` → `cancel_hold_invoice(parent)` and
     release.
   - If `slashed_share_sats == parent_bond_amount` → `settle` and payout
     the accumulated winnings (multiple counterparties supported by
     keeping child rows with their own `payout_invoice`).
   - If partial → there is no way to claim exactly the slashed sats from
     a single HTLC; we must choose between:
     - **(a)** Claim the whole bond, pay out the slashed share to
       counterparties, and refund the unslashed share back to the
       maker via `add-invoice` (maker becomes a counterparty here).
       Reuses Phase 3 plumbing.
     - **(b)** Never lock a parent bond; require a per-child bond at
       take time. Simpler to reason about but breaks the issue's
       requirement that the maker bond exists **before** the order is
       visible.

We recommend **(a)**. Acceptable cost: maker sees one extra `add-invoice`
DM at range-close if there were partial slashes.

2. **Fallback on HTLC expiry.** If the range order is still active when
   the hold invoice CLTV is about to expire, the scheduler must settle
   or cancel before LND does it for us. `hold_invoice_cltv_delta` in
   settings bounds this. Document the operator impact.

### 11.3 Scope

- Parent/child bond rows and helpers.
- Range-order publication sizes bond against `max_amount`.
- Child slash (from Phase 2 dispute or Phase 4/7 timeout): creates child
  row in `PendingPayout`; does not touch the parent HTLC yet.
- Parent close: resolve according to strategy (a) above.
- Extensive tests:
  - No slashes → full release to maker on close.
  - One small child slashed → pay counterparty, refund unslashed to
    maker.
  - Multiple child slashes across the range → each counterparty paid,
    residual refunded.
  - Cancellation during active children → pending slashes still
    actionable after.

### 11.4 Acceptance

- Issue invariant from §"Range Orders" satisfied: proportional slash,
  full release on close, child independence.
- No path exists that settles the parent hold invoice before the range
  is resolved.

---

## 12. Phase 7 — Maker timeout slash

Gate: `enabled && slash_on_waiting_timeout && apply_to ∈ { create, both }`.

Symmetric to Phase 4. Reuses §9.2's buyer/seller responsibility table —
this phase simply makes the lookup find a maker bond when the
responsible party is the maker. No new mechanism; the dispatch in
`job_cancel_orders` already exists, only the `apply_to` gate widens.

Keeps the Phase 4 invariant: cancels before timeout always release.

For range orders, a per-child timeout slashes the child's share via the
Phase 6 partial-slash path.

Tests mirror Phase 4 from the maker side; the "no slash" rows in the
§9.2 table become "slash maker bond".

---

## 13. Phase 8 — Public exposure + docs

### 13.1 Scope

- Extend the Mostro info event (`src/nip33.rs::info_to_tags`) with the
  bond config snapshot so clients can show users what the node enforces
  before they trade:
  - `bond` (`enabled` | `disabled`)
  - `bond-apply-to` (`take` | `create` | `both`)
  - `bond-slash-timeout` (`true` | `false`)
  - `bond-amount-pct` / `bond-amount-floor`
  - **No `bond-slash-dispute` tag**: dispute slashes are solver-driven
    per resolution, not a node-policy switch.
- Update `docs/admin_settle_order.html` and `admin_cancel_order.html`
  upstream (`mostro.network/protocol/`) with an "Optional payload —
  bond resolution" section showing the `BondResolution` shape and the
  four wire-format examples from §7.2.
- README + `docs/ARCHITECTURE.md`: add the bond flow to the per-action
  table and to the sequence diagrams. Include the §3.1 axes note.
- `docs/LIGHTNING_OPS.md`: operator runbook section for bonds (how to
  read audit events, how to resolve a `Failed` bond manually, what
  `BondResolution` looks like on the wire so an operator can read solver
  decisions in logs).
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
- New `Status` / `Action` / `Payload` variants in `mostro-core` (Phases
  1, 2, 5) must ship in that crate first and be pinned to a version in
  this repo's `Cargo.toml`. Clients must handle unknown statuses
  gracefully — this is already the case.
- An admin/solver client that does not yet know about `BondResolution`
  sends `payload: null`, which the daemon interprets as
  "release-by-default". No silent slashes.

### 14.3 Protocol/tag changes

Per `CONTRIBUTING.md § Protocol / Tag Changes`, each PR introducing
these requires a compatibility statement:

- New `Payload::BondResolution` variant in mostro-core (Phase 2). Minor
  version bump.
- New status `WaitingMakerBond` (Phase 5). The taker-side equivalent
  (`WaitingTakerBond`) is deferred per the Phase 1 implementation note;
  the current shipped code reuses `Pending`.
- New info-event tags (Phase 8).

### 14.4 Testing discipline

- Unit tests per phase as listed above, co-located under
  `#[cfg(test)] mod tests` in the touched module.
- Integration-style tests against an in-memory SQLite (see existing
  patterns in `src/util.rs::tests`).
- Manual LND regression checklist in each PR body:
  - lock a bond on polar/regtest
  - release via normal flow
  - release via cancel
  - (from Phase 2) slash via solver `BondResolution` covering each of
    the four `(slash_seller, slash_buyer)` combinations
  - (from Phase 3) counterparty receives payout
  - (from Phase 4) slash via timeout

### 14.5 Observability

- `tracing` spans in each bond transition with
  `bond_id`, `order_id`, `role`, `state`.
- For Phase 2 transitions, log the originating
  `BondResolution { slash_seller, slash_buyer }` so operators can audit
  solver decisions without reconstructing them from event traffic.
- A structured log line on every state change is enough; no Prometheus
  wiring needed until we see real traffic.

---

## 15. Why slash is decoupled from trade outcome

An earlier draft of this spec had a single configuration flag
`slash_on_lost_dispute` and assumed that "the loser of the dispute
resolution (settle ⇒ buyer wins, cancel ⇒ seller wins) is the party
deserving a bond slash". That equivalence breaks in real cases. This
section catalogues the cases that motivated the redesign.

### 15.1 Worked example — the "uncooperative party" case

- Alice is the maker of a sell-order; she is the **seller**.
- Bob takes it; he is the **buyer**.
- Alice opens a dispute claiming she never received the fiat from Bob.
- The solver investigates and agrees: no fiat was sent, so the trade
  must be cancelled (escrow back to Alice).
- The solver instructs Bob to initiate a cooperative cancel. Bob does.
- Alice does not co-sign the cancel.

The just outcome:
- Trade: cancel — Alice's escrow returns to her.
- Bonds: **Alice's bond should be slashed**, not Bob's. She is
  sabotaging a solver-blessed resolution, the very behaviour the bond
  exists to deter. Bob behaved correctly (he followed the solver's
  instruction at the cost of his own time and bond capital).

A pre-decoupling spec could only express *cancel + slash buyer (Bob)*
(because cancel ⇒ seller wins ⇒ buyer was the "loser") or *settle +
slash seller (Alice) + ship the trade money to Bob*. Both are wrong.
The decoupled `BondResolution` payload lets the solver express
*cancel + `{ slash_seller: true, slash_buyer: false }`* directly.

### 15.2 Other cases the decoupling unlocks

- **Both parties go silent during a dispute.** Solver may cancel and
  slash both bonds, or cancel + slash neither if it judges this a
  genuine outage rather than abuse.
- **Ambiguous evidence.** Settle/cancel with no slash, honouring "when
  in doubt, release" (§2.5). The earlier flag-driven model could not
  express this without disabling slashes node-wide.
- **Symmetric to §15.1.** Bob (buyer) opens a dispute, evidence shows
  Bob is honest, Alice (seller) refuses to settle. Solver issues
  *settle + `{ slash_seller: true, slash_buyer: false }`*. The flag-
  driven model would slash the loser of the resolution (Alice via
  settle), which happens to be correct here — but only by coincidence,
  not because the model captured the reasoning.

### 15.3 Why timeouts stay automatic

Timeouts are deterministic single-party failures: only one party owes
the action that the waiting-state names. There is no investigation
step. The §9.2 table produces the slash decision automatically and
correctly without solver involvement, so `slash_on_waiting_timeout`
remains a node-policy boolean.

---

## 16. Tracking

Each phase ships as a separate PR that links this document. The PR
description must state: which phase, which gate flags it touches, and
the manual LND/regtest evidence that the bond behaved correctly.

When the full plan has landed, this spec is kept in `docs/` as the
feature's reference.
