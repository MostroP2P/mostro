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
- **Slashed** (claimed by Mostro and split between the node and the
  winning counterparty per `slash_node_share_pct`; the node share funds
  solver compensation, the remainder is paid out to the counterparty as
  before — see §15.4) under two unambiguous conditions:
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
8. **Bonds must not block the order book.** While a taker's bond is
   outstanding (between `Requested` and `Locked`), the order **must
   remain visible and takeable** to other potential takers — the
   published NIP-33 event continues to advertise it as `pending`
   under [NIP-69](https://nips.nostr.com/69)'s four-bucket model
   (`pending` / `canceled` / `in-progress` / `success`). A malicious
   or absent taker cannot indefinitely hold an order off the book by
   initiating a take and then never paying the bond. **Multiple
   `Requested` bonds may coexist on a single order** — each fresh
   take creates a new bond row alongside any prior `Requested`
   rows, and the **first bond to reach `Locked` wins**. At the
   moment of the winning lock, every other `Requested` bond on the
   order is cancelled (its hold invoice is released and the prior
   taker is notified with `Action::Canceled`) and only then does
   the order transition to a trade-flow status (`WaitingPayment` /
   `WaitingBuyerInvoice`, which map to NIP-69 `in-progress`). A
   malicious taker who never pays does not block anyone: their
   bond invoice expires on the LND-side timeout, and any
   concurrent taker can still race them by paying their own bond.
   Any internal status the daemon adds to distinguish "matched,
   awaiting bond" from "advertised, no taker yet" (e.g.
   `Status::WaitingTakerBond` in §6.5) must map to NIP-69
   `pending` in `nip33::create_status_tags`, so external observers
   still see an available order.

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
#   "take" → only the taker posts a bond
#   "make" → only the maker posts a bond
#   "both" → both sides
apply_to = "take"

# Automatic slash on waiting-state timeout. Only applies when the
# scheduler-driven timeout actually elapses; user-initiated and admin
# cancels never trigger this path.
slash_on_waiting_timeout = false

# Fraction of a slashed bond that the node retains. The remainder is
# paid out to the winning counterparty as before. The node share is
# meant to fund solver compensation for dispute work (see §15.4).
# 0.0 = full payout to counterparty (legacy behaviour);
# 1.0 = node keeps everything.
slash_node_share_pct = 0.5

# Bond payout retries (applies once a bond is slashed and Mostro needs
# a payout invoice from the winning counterparty for its share).
payout_invoice_window_seconds = 300
payout_max_retries           = 5

# How many days the winning counterparty has, from the moment the bond
# is slashed, to claim their share by submitting a payout bolt11. If
# the window elapses without an invoice ever being received, the bond
# transitions to `Forfeited` and the node retains the counterparty
# share too (long-stop forfeiture; see §15.4). Independent from
# `payout_max_retries`, which only governs `send_payment` attempts
# *once an invoice has been received*.
payout_claim_window_days = 15
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
  maker/taker enum. Phase 1's "concurrent taker bonds, first-to-lock
  wins" semantics and Phase 5's "publish-after-bond-locks" gating are
  genuinely maker/taker concerns because they are about *order-flow*
  actions that only one role can perform. The maker bond is 1-to-1
  with the order (there is only ever one maker); the taker side
  admits N concurrent `Requested` bond rows per order until one
  locks.
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
| 1.5 | Protocol cleanup: dedicated `Action::PayBondInvoice` + `Status::WaitingTakerBond` (retire the Phase 1 `PayInvoice` reuse) | 1 | pending |
| 2 | Solver-directed dispute slash via `BondResolution` payload (taker bond) | 1.5 | pending |
| 3 | Payout flow: `Action::AddBondInvoice` to winner, routing-fee estimation, retries | 2 | pending |
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
  - `BondState { Requested, Locked, Released, PendingPayout, Slashed, Forfeited, Failed }`
    — `Forfeited` is the long-stop terminal state for a slash whose
    counterparty never claimed their share within
    `payout_claim_window_days` (§8.1). Distinct from `Failed`, which
    is reserved for technical errors (e.g. `send_payment` never routes).
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
    -- Phase 3: portion of `amount_sats` that the node retains on slash
    -- (frozen at the moment the bond enters `PendingPayout` so a later
    -- config change or daemon restart cannot re-balance the split).
    -- NULL for any bond that never reached `PendingPayout`. The
    -- counterparty share is always derived as
    -- `amount_sats - node_share_sats` so they cannot drift.
    node_share_sats  integer,
    -- Phase 3: counts ONLY `send_payment` retries against an invoice
    -- the counterparty has already submitted. Bumped only by step 6
    -- of the §8.1 scheduler loop. `payout_max_retries` is checked
    -- against this counter alone — invoice-request messages do NOT
    -- count here (see `invoice_request_attempts` below).
    payout_attempts  integer not null default 0,
    -- Phase 3: counts how many `Action::AddBondInvoice` messages the
    -- scheduler has sent asking the counterparty for a payout invoice.
    -- Bumped by step 1 of §8.1. Bounded by the forfeit window
    -- (`payout_claim_window_days`), not by `payout_max_retries`, so
    -- a slow-responding counterparty cannot prematurely flip the
    -- bond to `Failed`.
    invoice_request_attempts integer not null default 0,
    -- Phase 3: timestamp of the last `Action::AddBondInvoice` message. Drives the
    -- `payout_invoice_window_seconds` cadence check ("don't re-send
    -- before the window has elapsed"). Persisted so a daemon restart
    -- doesn't trigger an immediate re-send.
    last_invoice_request_at integer,
    locked_at        integer,
    released_at      integer,
    -- Set on entry to `PendingPayout` (i.e. when the slash decision is
    -- made), not on the later `Slashed` transition. Anchors the
    -- `payout_claim_window_days` forfeit deadline (§8.1).
    slashed_at       integer,
    created_at       integer not null,
    -- Phase 1 concurrent-bonds taker context. Stashed here while
    -- multiple `Requested` taker bonds race to `Locked`; the winner's
    -- columns are copied onto the `orders` row at lock-time. All
    -- nullable: maker bonds (Phase 5+) and child slash rows (Phase 6)
    -- leave them at NULL.
    taker_identity    char(64),
    taker_trade_index integer,
    taker_invoice     text,
    taker_fiat_amount integer,
    taker_amount      integer,
    taker_fee         integer,
    taker_dev_fee     integer,
    FOREIGN KEY(order_id) REFERENCES orders(id)
  );
  CREATE INDEX IF NOT EXISTS idx_bonds_order_id ON bonds(order_id);
  CREATE INDEX IF NOT EXISTS idx_bonds_state    ON bonds(state);
  CREATE INDEX IF NOT EXISTS idx_bonds_parent   ON bonds(parent_bond_id);
  ```

  The Phase 0 migration lands the full column set up-front
  (parent/child range columns, `slashed_share_sats`,
  `payout_routing_fee_sats`, `node_share_sats`,
  `invoice_request_attempts`, `last_invoice_request_at`, and the
  Phase 1 concurrent-bonds `taker_*` columns
  — `taker_identity`, `taker_trade_index`, `taker_invoice`,
  `taker_fiat_amount`, `taker_amount`, `taker_fee`, `taker_dev_fee`)
  rather than staging ALTER TABLEs per phase. Later phases only add
  code, not schema.

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
  from the trade hold invoice that follows). The dedicated
  `Status::WaitingTakerBond` / `Action::PayBondInvoice` lands in
  Phase 1.5 (§6.5) — that is the migration path; clients should treat
  the Phase 1 reuse as transitional. See §6.3 for the contract clients
  must respect during the Phase 1 window.
- Bond release is wired into every Phase 1 exit:
  `release_action`, `cancel_action` (cooperative + unilateral, taker- and
  maker-side, including pending-order maker cancels), `admin_settle_action`,
  `admin_cancel_action`, and `scheduler::job_cancel_orders`. Slashing
  hooks are intentionally absent and land in Phase 2+.
- `take_buy_action` / `take_sell_action` originally shipped with
  `bond::supersede_prior_taker_bonds`, which cancelled any prior
  `Requested` bond at retake time. Phase 1.5 (§6.5) replaces that
  with **concurrent taker bonds**: a fresh take creates a new bond
  row alongside any prior `Requested` rows; the **first bond to
  reach `Locked` wins** and the cancellation of the losers happens
  at lock-time, not at retake-time. A `Locked` prior bond still
  blocks new takes with `PendingOrderExists`. This implementation
  note is preserved for historical context — once Phase 1.5 ships,
  `supersede_prior_taker_bonds` is removed and the take handler's
  only pre-persist check is "is any bond on this order already
  `Locked`?".
- `cancel_action` recognises a bonded taker as authorised to cancel a
  still-pre-trade order: when `event.sender` matches the `pubkey` of an
  active `Requested` bond on the order, the cancel routes through the
  existing `cancel_order_by_taker` flow. Under Phase 1's
  one-bond-at-a-time invariant this releases the sole bond and resets
  the taker fields; under Phase 1.5's concurrent-bonds semantics it
  releases **only the sender's own bond** (other concurrent takers'
  bonds keep their HTLCs alive — they did not cancel) and the order
  stays in `WaitingTakerBond` if other `Requested` bonds remain. This
  lets a taker who took the order but no longer wants to proceed back
  out cleanly instead of getting `IsNotYourOrder`.
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

### 6.3 Client UX considerations (Phase 1 reality)

Because Phase 1 reuses `Action::PayInvoice` for the bond bolt11 (the
trade hold invoice that follows uses the same action), clients must
deliberately handle two cases this creates. Phase 1.5 (§6.5) replaces
this with a dedicated action type — until then, this section is the
contract.

The two cases:

- **Taker is the buyer (sell-order taken).** The taker receives a
  single `PayInvoice` (the bond) — they don't lock a trade hold
  invoice because they're receiving sats, not sending them. No
  ambiguity.
- **Taker is the seller (buy-order taken).** The taker receives **two
  `PayInvoice` actions in sequence on the same order**: the bond
  (typically ~1% of the trade, ≥ `base_amount_sats`) first, and once
  the bond HTLC is `Accepted`, the trade hold invoice (the full trade
  amount). They are emitted **sequentially, never simultaneously** —
  Mostro waits for the bond to lock before triggering the trade flow.

**Non-blockability invariant — important for UX.** Between the take
request and the bond locking, the order's published NIP-33 status
stays `pending` (per §2 principle 8). This is deliberate: a taker who
never pays the bond bolt11 cannot park the order off the book. Other
users browsing the order book continue to see it as available, and
any of them may attempt a fresh take.

Under Phase 1's original semantics, a fresh take cancelled prior
`Requested` bonds immediately via `supersede_prior_taker_bonds`, so
a slow taker received `Action::Canceled` as soon as anyone else
pressed "take". Phase 1.5 (§6.5) switches to **concurrent taker
bonds**: prior bonds stay alive — every concurrent taker keeps a
valid, payable bond invoice — and the first to actually pay (reach
`Locked`) wins. Only at that point are the losing concurrent
`Requested` bonds cancelled and their takers notified with
`Action::Canceled`. The TTL on each LND hold invoice still ensures
a malicious taker who never pays cannot block the order book
indefinitely.

What this means for clients in practice:

- A client that sent `take-buy` / `take-sell` and is waiting for
  `pay-bond-invoice` may receive `Action::Canceled` instead — meaning
  another concurrent taker locked their bond first. Surface this
  clearly: "Order was taken by another user before your bond was
  paid." Don't retry the take silently; the order may not be
  available anymore.
- Re-emitting `take-buy` / `take-sell` from the same pubkey while
  the client's bond is still `Requested` is idempotent: the daemon
  returns the same bolt11 instead of creating a second row for the
  same taker. Treat duplicate `pay-bond-invoice` messages on the
  same order as a re-send of the original, not a new bond.
- Don't gray out / hide the order from the local order-book view just
  because the user initiated a take. Until `Locked`, the order is
  still genuinely available to everyone.

Until Phase 1.5 lands, clients have four ways to distinguish bond
from trade invoice (in order of authoritativeness):

1. **bolt11 memo.** The bond is created with memo
   `"mostro bond order_id=<uuid>"` (§6.1). Authoritative but requires
   the client to decode the bolt11.
2. **Local state.** A client that already issued a `PayInvoice` for an
   order knows the next one is the trade escrow.
3. **Order status transition.** While the bond is outstanding the
   order remains in `Status::Pending`; once the bond locks, the order
   transitions to `WaitingPayment` (buy-order) or `WaitingBuyerInvoice`
   (sell-order — irrelevant here since the taker is buyer in that
   case). Clients subscribed to the order's NIP-33 events can use the
   transition as a boundary.
4. **Amount heuristic.** Bond = `max(amount_pct * order, base_amount_sats)`,
   typically ~1–2% of the trade. Useful as a sanity check, not
   authoritative.

Recommended client behaviour during the Phase 1 window:

- On take, if the operator's node has bonds enabled (visible once
  Phase 8 ships info-event tags; until then, out-of-band), surface to
  the user *before* they commit that this trade may require two hold
  invoices and what each one represents.
- On the first `PayInvoice` for an order, decode the memo and label
  the bond explicitly ("Anti-abuse bond: <amount> sats — locked, not
  spent"). Do not optimistically merge the two `PayInvoice` messages
  into a single UI flow; they are independent HTLCs and the user
  must approve each.
- If the bond invoice is paid but the daemon never sends the trade
  hold invoice (e.g. relay loss, daemon restart), the bond is
  released by `bond::resubscribe_active_bonds` on restart or by the
  scheduler timeout. Clients should treat the take as "stalled, will
  resolve" rather than retrying take-buy/take-sell — under
  Phase 1.5 a re-emit from the same pubkey returns the same bolt11
  (idempotent), under Phase 1 it would have raced with
  `supersede_prior_taker_bonds` and reset the bond.

These behaviours stay correct after Phase 1.5; the new action type
just means method (1) becomes "match on `Action::PayBondInvoice`"
instead of memo parsing.

---

## 6.5. Phase 1.5 — Dedicated `PayBondInvoice` + `WaitingTakerBond`

Small, protocol-only phase. Lands the dedicated `Action` and `Status`
variants that Phase 1 deferred (§6 implementation note) so clients can
route bond invoices by action type instead of by memo or status
heuristics. No new behaviour, no new slashing, no database schema
changes — pure ergonomics.

Lands **before Phase 2 on purpose**. Phase 2 introduces dispute
slashes and the `BondResolution` payload; once that ships and
operators flip `enabled = true` in production, every taker on every
bond-enabled node sees the bond message. We want clients to have already
adopted the clean API by then so the seller-as-taker case (§6.3)
never has to lean on memo parsing in the wild.

### 6.5.1 Scope

- **`mostro-core` 0.11.0** ships the two additive variants this phase
  needs (released — no further upstream work required):
  - `Status::WaitingTakerBond` — daemon-level status meaning "this
    order has a taker mid-bond". Distinguishes "matched, awaiting
    bond" from `Pending` ("advertised, no taker yet") for the
    daemon's own routing/persistence. Per NIP-69 only the four
    public buckets (`pending` / `canceled` / `in-progress` /
    `success`) appear in published order events anyway, so the
    mapping is what matters: `WaitingTakerBond` must map to NIP-69
    `pending` in `nip33::create_status_tags` (§2 principle 8).
  - `Action::PayBondInvoice` — Mostro is delivering a bond bolt11 for
    the taker to pay. Wire format identical to `Action::PayInvoice`,
    only the action discriminator differs. Name is deliberately
    `Pay…` not `Add…`: it follows the existing `PayInvoice`
    convention (Mostro → user, "pay this bolt11"); `Add…` would
    conflict with `Action::AddInvoice`'s established direction (user
    → Mostro, "here's my payout bolt11").
  - Both variants are serde-additive — older clients (still on
    `mostro-core` 0.10.x) that ignore unknown variants stay
    backward-compatible at the protocol layer.
- **`mostrod` changes** in `src/app/bond/flow.rs::request_taker_bond`
  and the take handlers:
  - Replace `Action::PayInvoice` with `Action::PayBondInvoice` when
    enqueuing the bond message.
  - Set the order's status to `Status::WaitingTakerBond` while the
    bond is outstanding (instead of leaving it in `Pending`). Add
    the NIP-69 mapping in `nip33::create_status_tags`:
    `WaitingTakerBond` → `(true, Status::Pending)` — same bucket as
    `Pending` itself, so the published event keeps advertising the
    order as available. This is the load-bearing piece for the
    non-blockability invariant; tests must lock it down.
  - `take_buy_action` / `take_sell_action` must accept takes against
    orders in **either** `Pending` or `WaitingTakerBond` — both are
    pre-trade states from the take-validation perspective.
  - **Switch from supersede to concurrent bonds.** Phase 1's
    `bond::supersede_prior_taker_bonds` helper is removed. The take
    handler's pre-persist check shrinks to:
    1. If `find_active_bonds_for_order` returns any bond in
       `BondState::Locked`, reject the take with `PendingOrderExists`
       (someone has already paid; the order is committed).
    2. If the sender's own pubkey already has a `Requested` bond on
       this order, return the existing `payment_request` instead of
       creating a new row — idempotent retry.
    3. Otherwise, create a fresh `Requested` bond row alongside any
       prior `Requested` rows from *other* pubkeys. The other rows
       are **not** touched.
    The take handler must also stop persisting taker-flow fields
    (`buyer_pubkey` / `seller_pubkey`, `master_*_pubkey`,
    `buyer_invoice`, `trade_index_*`, range-order `fiat_amount` /
    `amount` / `fee` / `dev_fee`) directly to the `orders` row
    while the order is in `WaitingTakerBond`. Those fields go on
    the bond row instead, in the `taker_*` columns of the `bonds`
    table (folded into the Phase 0 `CREATE TABLE`):
    `taker_invoice`, `taker_trade_index`, `taker_identity`,
    `taker_fiat_amount`, `taker_amount`, `taker_fee`,
    `taker_dev_fee` — all nullable so maker bonds (Phase 5+) and
    child slash rows (Phase 6) leave them at `NULL`. They are
    copied into the `orders` row by `resume_take_after_bond` at
    the moment the winning bond locks, so the order has no "ghost"
    taker while N concurrent bonds are racing.
  - **First-to-lock-wins resolution.** `on_bond_invoice_accepted`
    becomes the cancel-the-losers chokepoint. The `Requested → Locked`
    UPDATE gains a `NOT EXISTS (SELECT 1 FROM bonds WHERE order_id = ?
    AND state = 'Locked' AND id != ?)` guard so exactly one bond
    can win per order — if two `Accepted` events arrive in the same
    window, the losing UPDATE returns `rows_affected = 0` and the
    handler cancels its own HTLC with `cancel_hold_invoice` (the
    hold invoice is still cancelable: Mostro has not released the
    preimage yet) and notifies its taker with `Action::Canceled`.
    Once a bond does win, the handler iterates every other still-
    `Requested` bond on the order, calls `release_bond` on each
    (LND hold-invoice cancel + `BondState::Released`), and messages
    each loser an `Action::Canceled`. Only after this cleanup does
    it copy the winning bond's `taker_*` context onto the order
    and call `resume_take_after_bond`.
  - **Schema.** The `taker_*` columns above live directly in the
    Phase 0 `bonds` `CREATE TABLE` (no follow-up migration). The
    bond feature is not yet in production, so the schema is
    declared in its final shape at the Phase 0 migration rather
    than evolved by ALTER TABLEs across phases.
  - **New DB helper.** `find_active_bond_by_taker(pool, order_id,
    taker_pubkey) -> Option<Bond>` filtering on `state IN
    ('Requested', 'Locked')` and `pubkey = ?`. Used by the
    idempotent retry check above and by `cancel_action` (next
    bullet).
  - `cancel_action` must treat `WaitingTakerBond` as an alias of
    `Pending` for routing decisions. The bond is outstanding but the
    trade flow has **not** started, so the cooperative-cancel logic
    used for `WaitingPayment` / `WaitingBuyerInvoice` does NOT apply.
    Concretely, `cancel_action_generic` in `src/app/cancel.rs`
    today opens with `if order.check_status(Status::Pending).is_ok()
    { … }`; that guard widens to `if status ∈ { Pending,
    WaitingTakerBond }`. Inside the branch the existing two-route
    logic stays:
    - **Maker self-cancel.** `cancel_pending_order_from_maker` runs
      and the order publishes as `Status::Canceled`. **Every** active
      bond row on the order (all concurrent prospective takers) is
      released as part of this — the release hook is already wired
      into the maker-cancel path in Phase 1; only the status guard
      needs to widen.
    - **Taker self-cancel.** `cancel_order_by_taker` releases
      **only the sender's own bond** (looked up with
      `find_active_bond_by_taker`). If other prospective takers
      still have `Requested` bonds on the order, the order stays
      in `WaitingTakerBond` and is re-published. If the cancelling
      taker was the last one, the order drops back to `Pending`.
      External observers see no change in NIP-33 status either way
      (it was `pending` throughout per §2 principle 8). Since no
      taker fields are persisted on the order during
      `WaitingTakerBond` (per the concurrent-bonds rework above),
      there are no "taker fields to clear" — the cancel only
      releases the bond and republishes status.
    Without this widening the daemon falls through to the default
    `_ => NotAllowedByStatus` arm and rejects every cancel during
    the bond window — a regression vs. Phase 1, where the same
    flows work because the order is still in `Pending`. Tests for
    both routes must land alongside the status-guard widening so
    this stays locked down.
  - On bond `Locked`, transition `WaitingTakerBond` →
    `WaitingPayment` / `WaitingBuyerInvoice` as the existing trade
    flow does (and republish NIP-33 with the real new status — at
    that point the order is genuinely no longer takeable). The
    winning bond's `taker_*` columns are copied onto the order
    row in the same DB transaction so the trade flow sees a
    consistent snapshot.
  - On bond release before lock (taker abandons, taker self-cancel,
    or losing the lock race): if **no** other active bond remains on
    the order, transition `WaitingTakerBond` → `Pending` and
    republish. If other `Requested` bonds remain (a fresh concurrent
    taker is still in flight), leave the order in `WaitingTakerBond`.
  - The trade hold invoice continues to ship as `Action::PayInvoice`
    — only the bond switches.
- **Bump the `mostro-core` pin** in this repo's `Cargo.toml` from
  `0.10.0` to `0.11.0` so the new variants are reachable from
  `mostrod`. The Phase 1.5 PR is the natural place for this bump.

### 6.5.2 Client compatibility

- A client that only knows `Action::PayInvoice` will silently ignore
  the bond message after this phase ships, the bond will never lock, and
  the take will time out and release. No funds at risk, but the take
  fails. This is the expected behaviour for unknown-action handling
  per §14.2 ("Clients must handle unknown statuses gracefully") — it
  is also why Phase 1.5 lands before Phase 2: operators who flipped
  `enabled = true` *only after* Phase 1.5 see fail-fast behaviour
  rather than ambiguous `PayInvoice` mishandling.
- Operators are responsible for not flipping `enabled = true` in
  production until clients in the wild have adopted `mostro-core`
  0.11.0 (the release that ships `Action::PayBondInvoice` /
  `Status::WaitingTakerBond`). Phase 8 (§13.1) gives clients the
  `bond = enabled | disabled` info-event tag so they can warn users
  ("this node requires a bond your client doesn't support") instead
  of silently failing takes.

### 6.5.3 Tests

- Bond message enqueues with `Action::PayBondInvoice`, not `PayInvoice`.
- Order's **DB** status flips to `WaitingTakerBond` while the bond is
  outstanding, flips out to `WaitingPayment` /
  `WaitingBuyerInvoice` (per the existing trade flow) once the bond
  locks, and falls back to `Pending` on bond release before lock.
- **Non-blockability test (load-bearing) — concurrent bonds.** With
  order in status `WaitingTakerBond` and bond A in `Requested`:
  - `nip33::create_status_tags` returns `(true, Status::Pending)` —
    i.e. the order publishes in NIP-69's `pending` bucket, identical
    to the wire output for an order in `Pending`.
  - A second `take-buy` / `take-sell` from a different pubkey is
    accepted: bond A **remains** in `Requested` (its hold invoice
    is *not* cancelled), bond B is created in `Requested`,
    `find_active_bonds_for_order` returns `[A, B]`, and bond A's
    taker receives **no** `Action::Canceled` at this point.
  - A third concurrent take from a third pubkey is also accepted,
    producing bond C. `find_active_bonds_for_order` returns
    `[A, B, C]`. The order's status remains `WaitingTakerBond`
    throughout.
  - Re-emitting `take-sell` from bond A's pubkey while A is still
    `Requested` is idempotent: no new row is created, the same
    `payment_request` is re-sent on `Action::PayBondInvoice`.
  - When bond A's hash receives `InvoiceState::Accepted`: A
    transitions to `Locked`; bonds B and C transition to
    `Released` with their hold invoices cancelled via
    `cancel_hold_invoice`; B's and C's takers each receive
    `Action::Canceled`; A's `taker_*` columns are copied onto the
    `orders` row; the order transitions to `WaitingPayment` /
    `WaitingBuyerInvoice` as the existing trade flow does.
  - The order's NIP-69 bucket never leaves `pending` until a
    bond actually `Locked` (regardless of how many concurrent
    takers come and go).
- **Locked-bond gate.** Once any bond on an order reaches `Locked`,
  any subsequent `take-buy` / `take-sell` (from any pubkey,
  including the locking taker's) is rejected with
  `PendingOrderExists`. Concurrent `Requested` bonds are no longer
  permitted at that point — the trade is committed.
- **Range order: per-bond pricing.** For a range order with
  `price_from_api = true`, taker A takes at quote X. 30 seconds
  later, taker B takes at quote Y (Y ≠ X because the rate moved).
  Both bonds are `Requested`. When A's bond locks, the order's
  `amount` / `fee` / `dev_fee` / `fiat_amount` are populated from
  A's `taker_*` columns — i.e. quote X. Y is discarded along with
  bond B. Confirms each bond carries its own pricing snapshot
  rather than racing to mutate the `orders` row at take time.
- **Lock-race: two `Accepted` events in the same window.** With
  bonds A and B both in `Requested`, fire `on_bond_invoice_accepted`
  for both back-to-back. The conditional UPDATE's `NOT EXISTS`
  guard ensures exactly one transition to `Locked` succeeds. The
  loser's handler observes `rows_affected = 0`, calls
  `cancel_hold_invoice` on its own hash to release its taker's
  HTLC without settling, and messages `Action::Canceled` to its taker.
  Net effect is identical to the staggered case; no double-lock
  and no settled HTLC for the loser.
- Seller-as-taker case: one `PayBondInvoice` followed by one
  `PayInvoice` on the same order, both visible on the wire as
  distinct action types — no memo parsing needed by the client.
- **Cancel during `WaitingTakerBond` — taker self-cancel, lone
  taker.** With the order in `WaitingTakerBond` and exactly one
  prospective taker's bond in `Requested`, the taker sends a
  `cancel` action. The daemon releases that single bond,
  transitions the order back to `Pending`, and republishes —
  `cancel_action` returns `Ok(())` (not `NotAllowedByStatus`).
  The published NIP-33 status was `pending` throughout, so
  external observers see no transition; the order remains
  takeable.
- **Cancel during `WaitingTakerBond` — taker self-cancel, others
  still active.** With the order in `WaitingTakerBond` and bonds
  A and B both in `Requested`, A's taker sends `cancel`. Bond A
  is released; bond B remains in `Requested` and continues racing.
  The order **stays** in `WaitingTakerBond` (does not drop to
  `Pending` because B is still active). No `Action::Canceled` is
  sent to B's taker. Confirms taker self-cancel is scoped to the
  sender's own bond only.
- **Cancel during `WaitingTakerBond` — maker self-cancel.** With the
  order in `WaitingTakerBond` and one or more concurrent prospective
  takers mid-bond, the maker sends a `cancel` action. The daemon
  runs `cancel_pending_order_from_maker`: the order transitions to
  `Status::Canceled`, **every** active prospective taker's
  `Requested` bond is released, every prospective taker receives
  `Action::Canceled`, and the NIP-33 event republishes with
  `s = canceled`. Confirms maker-side cancel is not blocked by
  pending bonds and fans out cancellation to all concurrent takers.
- **Cancel during `WaitingTakerBond` — third-party rejected.** A
  pubkey that is neither the maker nor a bonded taker on the
  order receives `IsNotYourOrder`. Same rejection semantics as the
  existing `Pending` branch; widening the status guard must not
  weaken the authorisation check.
- With `enabled = false`, neither the new action nor the new status
  is emitted; backward compatibility is preserved.

### 6.5.4 Acceptance

- The Phase 1 `Action::PayInvoice`-for-bond workaround is retired in
  this PR's diff. After this lands, mostrod only ever emits
  `PayInvoice` for trade hold invoices and `PayBondInvoice` for
  bonds.
- The §6.3 client-side memo parsing recommendation becomes
  unnecessary; clients dispatch on action type alone.
- The §2 non-blockability invariant is preserved: orders in
  `WaitingTakerBond` map to NIP-69 `pending` and re-take from a
  different pubkey is accepted. The mechanics change from Phase 1
  (immediate supersede of the prior `Requested` bond) to
  concurrent bonds (the prior `Requested` bond stays alive; the
  first to `Locked` wins and cancels the losers at lock-time).
- `bond::supersede_prior_taker_bonds` is removed from
  `src/app/bond/flow.rs`. The take handler's only pre-persist
  check is "is any bond on this order already `Locked`?".
- Phase 2 can rely on the clean API when it ships.

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

### 7.1 `BondResolution` payload variant in `mostro-core`

`mostro-core` 0.11.0 ships `Payload::BondResolution`. From
`mostro-core::message::Payload`:

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

This is a serde-additive payload variant — not a breaking change to
the wire format. It is delivered as part of the `mostro-core` 0.11.0
bump that Phase 1.5 (§6.5.1) pulls into `Cargo.toml`; Phase 2 itself
adds no further upstream dependency change. See §14.3.

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
   `state = PendingPayout, slashed_reason = LostDispute, slashed_at = now`,
   and persist the split per §8.1 (compute `node_share_sats` from the
   current `slash_node_share_pct` and write it in the same DB update).
   Bonds not marked for slash are cancelled (`cancel_hold_invoice`) and
   marked `Released` immediately.

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

- Split computation, applied **once** at the moment a bond transitions
  to `PendingPayout` (`admin_settle_action` / `admin_cancel_action`
  in Phase 2; `job_cancel_orders` in Phase 4 / 7). The value is
  written to the `node_share_sats` column (declared in Phase 0's
  schema, NULL until first use) in the same DB write that flips the
  state, so it is genuinely frozen across daemon restarts and
  subsequent config changes:
  ```text
  node_share_sats          = floor(amount_sats * slash_node_share_pct)
  counterparty_share_sats  = amount_sats - node_share_sats
  ```
  Floor + subtraction guarantees the two shares sum exactly to
  `amount_sats` (no rounding leaks). `counterparty_share_sats` is
  never persisted — it is always derived as
  `amount_sats - node_share_sats` so the two cannot drift.

- New scheduler job `job_process_bond_payouts` in `src/scheduler.rs`,
  mirroring `job_process_dev_fee_payment`:
  - Polls `PendingPayout` bonds at a fixed interval (default 60s).
  - Reads `node_share_sats` from the row (never re-reads
    `slash_node_share_pct` from config — that lookup happens only at
    the slash transition above).
  - For each, **first check the forfeit window**:
    - If `now - slashed_at >= payout_claim_window_days * 86400` and
      `payout_invoice IS NULL` (the counterparty never submitted a
      bolt11): `settle_hold_invoice(preimage)`, transition the bond
      to `state = Forfeited`, publish the audit event with
      `outcome = forfeited`, and stop. The node retains the full
      `amount_sats`. No further messages or `send_payment` runs.
    - Otherwise, proceed with the normal payout steps below.
  - Normal payout steps:
    1. If no `payout_invoice` yet, and the cadence window has elapsed
       (i.e. `last_invoice_request_at IS NULL` or
       `now - last_invoice_request_at >= payout_invoice_window_seconds`):
       enqueue an `Action::AddBondInvoice` message (mostro-core 0.11.2+)
       to the recipient (see "Recipient resolution" below) asking for
       a bolt11 for the full `counterparty_share_sats` — the handler
       validates the invoice principal against `counterparty_share_sats`
       with fee = 0, so the routing fee must come out of Mostro's
       wallet, not the invoice principal. (The bonded user may use
       the estimated routing fee as guidance when choosing a recipient
       node, but it is not subtracted from the requested amount.)
       The message carries **only the structured request payload** — no
       hardcoded human-readable text. The forfeit deadline is not
       shipped inline: the client computes it locally from the slash
       moment (observable on the order's audit trail) plus
       `bond_payout_claim_window_days`, which Mostro advertises on the
       kind-38385 info event (§13.1). Keeping the wire payload
       text-free lets clients render the warning ("your share will be
       forfeited in N days") in the user's own locale, and avoids
       baking English copy into the daemon. Bump
       `invoice_request_attempts` and set `last_invoice_request_at =
       now`. **Do not touch `payout_attempts`** — that counter only
       governs `send_payment` retries (step 6); mixing the two would
       let a slow-responding counterparty exhaust `payout_max_retries`
       on invoice-request messages alone and prematurely flip the bond to
       `Failed`. Bounding invoice requests is the forfeit window's job
       (`payout_claim_window_days`), not the retry budget's. The node
       share never leaves Mostro's wallet, so it has no separate
       invoice step.
    2. If an invoice was received (see handler below), **estimate the
       routing fee** via `LndConnector::query_routes(dest, amount)`
       (thin wrapper over LND `router::query_routes`); fall back to
       `amount * max_routing_fee` if the RPC fails.
    3. `settle_hold_invoice(preimage)` on the bond hash to claim the
       forfeited sats into Mostro's wallet. After this call,
       `node_share_sats` is implicitly retained (it just stays in
       Mostro's wallet).
    4. `send_payment` to the counterparty invoice with capped fee.
       Only `counterparty_share_sats - routing_fee` leaves Mostro.
    5. On success → `state = Slashed`. (`slashed_at` is **not**
       touched here — it was set at the `PendingPayout` transition;
       see Phase 2 §7.3.)
    6. On `send_payment` failure → bump `payout_attempts` (this is
       the *only* place that increments it); once `payout_max_retries`
       reached, transition to `Failed` and leave a tracing error.
       `Failed` is reserved for *technical* failure (we have an
       invoice but can't route to it) and is distinct from
       `Forfeited` (the user never gave us an invoice).

  When `slash_node_share_pct = 1.0` the counterparty leg is skipped
  entirely (no `AddBondInvoice` message, no `send_payment`, no forfeit
  window to wait for); the bond goes straight from `PendingPayout` →
  settle → `Slashed` after step 3.

- **Recipient resolution.** Step 1 above sends `Action::AddBondInvoice`
  to the *non-slashed counterparty* of the trade — the party who is
  neither the bonded user (`bond.pubkey`) nor a co-slashed party.
  Because `BondResolution` flags are dispute-only and `bond.pubkey`
  is not enough on its own to recover the trade-flow side
  (buyer/seller), the rule is keyed on `slashed_reason`:
  - **`LostDispute` (Phase 2 / 5).** The solver's `BondResolution`
    flag named the side: `slash_seller=true` → seller's bond is in
    `PendingPayout`, recipient = buyer; `slash_buyer=true` →
    recipient = seller. Mapping buyer/seller → maker/taker → concrete
    pubkey uses the §3.1 order-kind table.
  - **`Timeout` (Phase 4 / 7).** No `BondResolution` payload exists.
    The slashed party is the one responsible for the elapsed waiting
    state per the §9.2 table: `WaitingBuyerInvoice` → buyer was
    responsible (and was slashed), recipient = seller;
    `WaitingPayment` → seller was responsible, recipient = buyer.
    Mapping uses the same §3.1 table.
  - **Both bonds slashed in a single dispute (Phase 5+ only).** When
    the solver's `BondResolution` sets both flags and both maker and
    taker have active bonds, neither party deserves restitution
    (§15.2 — "both behaved badly"). For each row, treat as
    `slash_node_share_pct = 1.0` for that payout: skip the
    `AddBondInvoice` message, retain `amount_sats` in full, settle the
    HTLC in step 3, transition to `Slashed`. (Phase 5+ wires this;
    Phase 2's taker-only world cannot reach this branch.)

- Late-invoice race: the `add_bond_invoice_action` handler (below)
  must check the bond is still in `PendingPayout` before persisting
  the `payout_invoice`. If the scheduler already transitioned the
  row to `Forfeited`, the late invoice is rejected with a localised
  message ("the claim window expired on <date>"). This is a clean
  per-row decision, no locks needed — the state column is the
  arbiter.
- New action handler `add_bond_invoice_action` in a new
  `src/app/bond/payout.rs` module. Receives an `Action::AddBondInvoice`
  reply from a bond-payout candidate. The dedicated action type
  (mostro-core 0.11.2+) keeps it disjoint from the buyer-invoice
  `add_invoice_action` at the routing layer rather than relying on
  ambient DB state. On accepting a valid
  invoice it persists `payout_invoice` and **resets
  `invoice_request_attempts = 0`** in the same DB write, marking a
  clean transition from the invoice-request phase to the
  `send_payment` phase. The reset isn't load-bearing for correctness
  (step 1's guard is `payout_invoice IS NULL`, so the counter
  naturally stops growing once an invoice is persisted), but keeps
  the row tidy and lets operators distinguish "nudges before
  invoice" from "nudges after a hypothetical re-prompt" if a future
  phase ever introduces one. Any "took N nudges to respond" logging
  for observability should capture the value *before* the reset.
- Unit tests: routing-fee fallback, retries exhaustion, settle-then-pay
  ordering.

### 8.2 Failure modes & invariants

- **`settle` must succeed before `send_payment`.** If settle fails we
  leave the bond in `PendingPayout` and retry on the next tick; the
  bonded party's HTLC stays held, which is the correct safety posture.
- **Node share is retained unconditionally on settle.** Once
  `settle_hold_invoice` succeeds, `node_share_sats` is in Mostro's
  wallet and the node leg is done. The only thing the retry loop is
  driving from that point on is the counterparty payout.
- **Partial success: settle OK, counterparty `send_payment` failed.**
  The bond state stays in `PendingPayout` with a best-effort retry. The
  winner is kept informed via periodic messages. If retries exhaust, state
  becomes `Failed`; at that point Mostro is holding the counterparty
  share (unavoidable with the HTLC already settled) and logs loudly.
  The node share is unaffected — it was always going to stay. This is
  a known limitation; node operators can manually pay the winner from
  logs.
- **Counterparty never claims (forfeit path).** Distinct from
  `Failed`: there is no `payout_invoice` because the counterparty
  never sent one. After `payout_claim_window_days` from `slashed_at`,
  the scheduler settles the HTLC and transitions to `Forfeited`. No
  manual operator action is needed — this is a normal terminal
  state, designed-in. Default 15 days gives even users with sporadic
  Nostr presence ample time to see the message and respond.
- **Non-blocking:** `release_action`, `admin_settle_action`, etc. return
  success the moment the trade escrow resolves. Bond payout happens
  later.

### 8.3 Acceptance

- End-to-end test: dispute resolved with `slash_buyer=true` and
  `slash_node_share_pct = 0.5` → buyer-side counterparty (the seller)
  is asked for a bolt11 sized at the full counterparty share
  (`amount_sats − floor(amount_sats * slash_node_share_pct)` =
  `floor(amount/2)`). The payout-invoice principal carries the
  counterparty share **only**; routing fee is paid separately from
  Mostro's own wallet (capped at `max_routing_fee` and recorded into
  `payout_routing_fee_sats`), not deducted from the requested
  principal. Submits it → bond payout settles, the two shares sum
  exactly to `amount_sats` (no rounding leak).
- Edge case `slash_node_share_pct = 0.0` → behaviour identical to the
  pre-split design (full counterparty payout).
- Edge case `slash_node_share_pct = 1.0` → settle the HTLC, no
  `AddBondInvoice` message is enqueued, no `send_payment` runs, bond goes
  straight to `Slashed`.
- **Persistence test**: a bond enters `PendingPayout` under
  `slash_node_share_pct = 0.5`; before payout completes, simulate a
  daemon restart with `slash_node_share_pct = 0.9` in the new
  config; payout still uses the original 0.5 split (read from
  `node_share_sats`). Same shape: change config back and forth
  during `PendingPayout` ticks → split is unaffected.
- **Forfeit window test**: bond enters `PendingPayout`, counterparty
  never replies; advance the test clock by
  `payout_claim_window_days + 1`; on the next scheduler tick the bond
  settles and transitions to `Forfeited`. Node retains `amount_sats`
  in full. No `send_payment` was ever attempted.
- **Late-invoice rejection**: bond is `Forfeited`; counterparty
  submits a bolt11 after the deadline; `add_bond_invoice_action`
  rejects it with a "claim window expired" message; bond stays
  `Forfeited`.
- **`Failed` vs. `Forfeited` distinction**: counterparty submits a
  valid invoice on day 1 but it never routes; after
  `payout_max_retries` the bond goes to `Failed` (not `Forfeited`,
  even though the 15-day window is still open). `Failed` requires
  operator attention; `Forfeited` does not.
- Retry test: counterparty submits an invoice that never routes →
  scheduler keeps retrying up to `payout_max_retries`, then `Failed`.
  Node share already retained; only the counterparty share is stuck.

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

Phase 7 fills the "no slash" rows for `apply_to ∈ { make, both }` by
adding maker bond rows to the lookup.

### 9.3 Scope

- Modify `scheduler::job_cancel_orders`: when the waiting-state timeout
  elapses on an order in `WaitingBuyerInvoice` / `WaitingPayment`, run
  the §9.2 lookup. If a bond exists for the responsible party, set
  `state = PendingPayout, slashed_reason = Timeout` (Phase 3 picks it
  up). Continue the existing cancel-escrow + republish work. The
  payout recipient is then resolved by Phase 3 per the "Recipient
  resolution" rule in §8.1: `slashed_reason = Timeout` plus the §9.2
  responsibility entry uniquely names the non-slashed counterparty
  (`WaitingBuyerInvoice` → seller; `WaitingPayment` → buyer).
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

Gate: `enabled && apply_to ∈ { make, both }`.

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
- Feature enabled, apply_to=make: order is not visible in the book
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
   - If `slashed_share_sats == parent_bond_amount` → `settle`, retain
     `node_share_pct` of every accumulated child slash in Mostro's
     wallet and pay out the counterparty share to each child's winner
     (multiple counterparties supported by keeping child rows with
     their own `payout_invoice`).
   - If partial → there is no way to claim exactly the slashed sats from
     a single HTLC; we must choose between:
     - **(a)** Claim the whole bond, pay out the per-child counterparty
       shares (each `floor(child_slash * (1 - node_share_pct))`), retain
       the per-child node shares, and refund the unslashed share
       back to the maker via `add-invoice` (maker becomes a
       counterparty here). Reuses Phase 3 plumbing.
     - **(b)** Never lock a parent bond; require a per-child bond at
       take time. Simpler to reason about but breaks the issue's
       requirement that the maker bond exists **before** the order is
       visible.

We recommend **(a)**. Acceptable cost: maker sees one extra `add-invoice`
message at range-close if there were partial slashes.

The split is applied **per child** (each child's `slashed_share_sats`
is split independently), so the audit event for each child carries its
own `node-share` and `counterparty-share` tags and the maker refund is
exactly `parent_bond_amount - sum_of_child_slashes` — the node share
of slashed children stays in Mostro's wallet, never gets refunded to
the maker.

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

Gate: `enabled && slash_on_waiting_timeout && apply_to ∈ { make, both }`.

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

- The Mostro info event (`src/nip33.rs::info_to_tags`) carries the
  bond config snapshot so clients can show users what the node
  enforces before they trade. **The full set below is shipped in
  Phase 3** alongside the payout flow itself — the
  `Action::AddBondInvoice` message intentionally carries no
  human-readable deadline text, so the wire payload alone is not
  enough for a client to warn the user; the kind-38385 tags close
  that gap and let every warning render in the user's locale. Tag
  naming follows the snake_case convention used elsewhere in
  `info_to_tags` (`mostro_version`, `hold_invoice_expiration_window`,
  etc.). The set:
  - `bond_enabled` (`true` | `false`) — **always emitted**, including
    on nodes where `[anti_abuse_bond]` is absent or `enabled =
    false`. Disambiguates "feature off on this node" from "older
    daemon that doesn't speak bond at all": the latter omits the tag
    entirely, the former emits `false`. All remaining bond tags are
    emitted only when this is `true`.
  - `bond_apply_to` (`take` | `make` | `both`) — whether the user
    needs to lock a bond as maker, taker, or both.
  - `bond_slash_on_waiting_timeout` (`true` | `false`) — node policy:
    can a bond be slashed for missing a waiting-state timeout, or
    only by solver directive in a dispute?
  - `bond_amount_pct` / `bond_base_amount_sats` — bond economics:
    `max(amount_pct * order_amount, base_amount_sats)`.
  - `bond_slash_node_share_pct` — fraction of a slashed bond retained
    by the node (the rest goes to the winning counterparty). Lets the
    user see up-front what they would actually receive.
  - `bond_payout_claim_window_days` — number of days the winning
    counterparty has, from `slashed_at`, to submit a payout invoice
    before forfeiting their share. Clients add this to `slashed_at`
    to render the deadline ("you have N days to claim") in the user's
    own locale; Mostro never ships that text inline on the
    `AddBondInvoice` message itself.
  - **No `bond_slash_dispute` tag**: dispute slashes are solver-driven
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
- New `Status` / `Action` / `Payload` variants in `mostro-core` must
  ship in that crate first and be pinned to a version in this repo's
  `Cargo.toml`. As of `mostro-core` **0.11.0**, the variants for
  Phases 1.5 and 2 (`Status::WaitingTakerBond`,
  `Action::PayBondInvoice`, `Payload::BondResolution`) are released
  and ready to pin. Phase 5's `Status::WaitingMakerBond` is still
  pending in `mostro-core`. Clients must handle unknown statuses
  gracefully — this is already the case.
- An admin/solver client that does not yet know about `BondResolution`
  sends `payload: null`, which the daemon interprets as
  "release-by-default". No silent slashes.

### 14.3 Protocol/tag changes

Per `CONTRIBUTING.md § Protocol / Tag Changes`, each PR introducing
these requires a compatibility statement:

- `Action::PayBondInvoice` + `Status::WaitingTakerBond` in mostro-core
  (Phase 1.5). **Released in `mostro-core` 0.11.0.** Retires the Phase
  1 reuse of `Action::PayInvoice` / `Status::Pending` for bonds.
- `Payload::BondResolution` variant in mostro-core (Phase 2).
  **Released in `mostro-core` 0.11.0** (same release).
- `Action::AddBondInvoice` in mostro-core (Phase 3). **Released in
  `mostro-core` 0.11.2.** Counterparty-direction dual of
  `Action::PayBondInvoice`; keeps the bond-payout reply disjoint from
  the buyer-invoice `Action::AddInvoice` so the daemon can route on
  action type alone.
- `Status::WaitingMakerBond` (Phase 5). Not yet shipped upstream;
  needs a follow-up `mostro-core` minor release before Phase 5 can
  land here.
- Info-event tags (Phase 8). No upstream dependency — daemon-side only.

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

### 15.4 Why slashes are split between node and counterparty

A pure "winner-takes-all" payout (the original §1 design — 100% to the
counterparty) doesn't fund the work that produced the slash decision
in the first place. Solvers spend real time reading evidence on a
dispute path; node operators carry hosting and Lightning liquidity
costs on the timeout path. If neither is funded, both roles are
volunteer-only, which doesn't scale.

The split solves this without introducing a new payment rail: the
slash already routes through Mostro's wallet (the HTLC must be settled
before any payout can happen — §8.1 step 3), so retaining a fraction
is free. `slash_node_share_pct` is the knob that decides what fraction
that is. Defaults to **0.5** as a reasonable starting point — half to
the wronged counterparty (preserves the deterrent: the cheater still
funds their victim), half to the node (funds solver compensation and
operations). Operators are free to set it elsewhere:

- `0.0` — legacy "winner takes all" behaviour. Choose this if solver
  compensation is handled out of band (e.g. a separate fee, donations,
  or volunteer solvers).
- `1.0` — node retains the entire slash. Only sensible when the bond
  is intended purely as a sybil/abuse cost and the operator does not
  want any redistributive component.
- `0.5` — recommended default; balances victim restitution against
  funding the dispute-handling work the bond exists to motivate.

The split being **public** (exposed in the Mostro info event per §13.1)
is important: a user must be able to see the policy before they
choose to lock a bond on a given node. A node that quietly raises
`slash_node_share_pct` after the fact would be visible to clients on
the next info refresh.

This change does not weaken the deterrent. The cheater still loses
the full bond; only the destination of the sats changes. From the
slashed party's perspective the cost is identical at any value of
`slash_node_share_pct`, which is what makes the bond function as a
disincentive in the first place (§2 principle 5: the threat must be
unambiguous to the *bonded* party; how Mostro then divides the
forfeited sats is an internal accounting decision).

The forfeit window (`payout_claim_window_days`, default 15) is the
long-stop tail of the same logic. If the wronged counterparty never
submits a payout invoice — because they lost their key, gave up on
the platform, or simply forgot — the sats cannot sit in
`PendingPayout` forever (the HTLC has a CLTV, and the node has a
real liability tracking unclaimed funds). After the window expires
the node retains the counterparty share too; the bond closes as
`Forfeited`. This keeps the on-chain accounting deterministic and
removes the "Mostro mysteriously holds X sats" failure mode without
manual intervention. From the cheater's side the deterrent is again
unaffected — they lost their bond either way. From the wronged
counterparty's side the message is clear (and surfaced in the info
event per §13.1): claim within N days or forfeit.

---

## 16. Tracking

Each phase ships as a separate PR that links this document. The PR
description must state: which phase, which gate flags it touches, and
the manual LND/regtest evidence that the bond behaved correctly.

When the full plan has landed, this spec is kept in `docs/` as the
feature's reference.
