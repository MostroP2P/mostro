# Cashu Escrow — Track D: Dispute Resolution (`P_M` signs)

**Status:** Draft for review · **Target:** `main` (**requires `mostro-core ≥ 0.14.0`**) ·
**Depends on:** Fundamentals **CF-1, CF-2, CF-5** + **Track A** (the escrow must be
locked before it can be disputed) + **Track C TC-1** (the shared fee-refund
helper, for TD-3) · **Feature flag:** `[cashu].enabled`

Track D is the **only** track where Mostro's arbitrator key `P_M` produces a
signature over the **escrow**. When a locked Cashu trade cannot be resolved
cooperatively, a solver decides the outcome and Mostro hands the **winner** its
`P_M` signature so the winner can complete a 2-of-3 swap and take the funds.
This closes the trust model: the 2-of-3 exists precisely so that a single honest
arbitrator can break a buyer↔seller deadlock without ever holding the funds.

This document assumes Fundamentals and Track A are merged. It only adds behaviour
*inside the Cashu branch*; the Lightning path is never changed.

---

## 1. Goal and scope

### Goal
Resolve a disputed locked Cashu trade by arbitrator signature:
1. A party opens a `Dispute`; a solver takes it (`AdminTakeDispute`).
2. The solver rules for one side:
   - **`AdminSettle`** (buyer wins) → Mostro signs with `P_M` and delivers the
     signature to the **buyer**, who redeems with `P_M + P_B`.
   - **`AdminCancel`** (seller wins) → Mostro signs with `P_M` and delivers the
     signature to the **seller**, who reclaims with `P_M + P_S`, and Mostro
     **refunds the seller the fee** (Track A §4A, via Track C's helper).
3. Mostro advances the order to its terminal dispute state **only when the
   outcome is enforceable** (§4B) and makes the outcome auditable.

### In scope
- `sign_with_pm` — the `P_M` signing primitive (NUT-11 P2PK) in `CashuClient`
  (shared with TA-1f's fee redeem and Track C's fee refund; whichever lands
  first carries it).
- The Cashu branches of `dispute_action`, `admin_take_dispute_action`,
  `admin_settle_action`, `admin_cancel_action`, delivering `CashuPmSignature`.
- Unblocking `Dispute`, `AdminTakeDispute`, `AdminSettle`, `AdminCancel`,
  `AdminAddSolver` in `dispatch_cashu`.
- The **near-locktime guard** (Track A §4B): solver alert on take, a hard
  pre-sign check on settle, and finalisation only on an observed spend.
- The **fee refund** on a seller-wins resolution (Track A §4A) through
  `refund_cashu_fee` (Track C §4).

### Out of scope (other tracks)
- Happy-path **release** → Track B. **Cooperative cancel** → Track C.
- Bond-related admin flows — bonds are mutually exclusive with Cashu (CF-1 §4.5),
  so `AddBondInvoice` stays permanently `InvalidAction`.

---

## 2. Where Track D sits — flow and state transitions

```mermaid
sequenceDiagram
    participant P as Party (buyer or seller)
    participant M as Mostro (cashu mode)
    participant Solver as Solver
    participant W as Winner
    participant Mint as Cashu Mint

    Note over P,M: Escrow locked (Track A); trade stalled
    P->>M: Dispute
    Solver->>M: AdminTakeDispute
    Note over M,Solver: solver alerted + deadline shown if remaining locktime < margin (§4B)
    Solver->>M: AdminSettle (buyer wins) | AdminCancel (seller wins)
    M->>Mint: check_state(escrow proof Ys)  [hard guard: must be UNSPENT]
    M->>M: sign_with_pm(escrow proofs)
    M->>W: CashuPmSignature { per-proof {secret, signature} }
    W->>Mint: SwapRequest {2-of-3 proofs, P_M sig + own sig, own outputs}
    Mint-->>W: funds to the winner
    M->>Mint: check_state  [release watcher, Track B §5C]
    Mint-->>M: SPENT before locktime -> CompletedByAdmin
    Note over M: if seller wins, refund_cashu_fee (Track C §4)
```

**State transitions Track D performs:** `Dispute` opens the dispute
(`Active/FiatSent/SettledHoldInvoice → Dispute`). `AdminSettle` drives
`Dispute → SettledByAdmin` (signature delivered to the buyer) and the Track B
release watcher drives `SettledByAdmin → CompletedByAdmin` once the escrow is
observed spent before the locktime — the exact Lightning sequence
(`admin_settle` settles, the payout completes). `AdminCancel` drives
`Dispute → CanceledByAdmin` directly: the seller can always reclaim (with `P_M`
before the locktime, alone after it), so nothing remains to observe. In every
case there is **no hold-invoice settle/cancel** — Mostro emits a signature
instead.

---

## 3. What Track D consumes from Fundamentals + Track A

| Needs | From | Exact item |
|-------|------|------------|
| Mode gate | CF-1 | `Settings::is_cashu_enabled()` |
| Locked escrow row | Track A | `Order.{cashu_escrow_token}` populated; the token's `locktime` |
| Settlement margin | Track B TB-1 | `escrow_settlement_margin_days` — reused as the resolution SLA (§4B) |
| `P_M` signing | CF-2 surface (TA-1f / TD-1) | `CashuClient::sign_with_pm(token \| proofs) -> Vec<CashuProofSignature>` |
| Proof state at the mint | CF-2 | `CashuClient::check_state(ys)` (NUT-07) — the pre-sign guard and the watcher's source of truth |
| Release watcher | Track B TB-2 | `SettledHoldInvoice → Success` job, extended to `SettledByAdmin → CompletedByAdmin` |
| Winner pubkey | existing | `order.get_buyer_pubkey()` / `get_seller_pubkey()` |
| Fee refund (seller wins) | Track C TC-1 | `refund_cashu_fee(order)` — the single-shot contract of Track C §4.3 |
| Solver management | existing | `admin_add_solver`, `admin_take_dispute` |
| Dispatch seam | CF-5 | `Dispute`/`AdminTakeDispute`/`AdminSettle`/`AdminCancel`/`AdminAddSolver` arms |

Protocol (already on `main`, `mostro-core ≥ 0.14.0`, frozen):
- `Action::CashuPmSignature` (Mostro → winner) carrying
  `Payload::CashuSignatures(Vec<CashuProofSignature>)`, where
  `CashuProofSignature = { secret, signature }` — **one entry per escrow proof**.
- `Action::{Dispute, AdminTakeDispute, AdminSettle, AdminCancel, AdminAddSolver}`
  and the dispute payloads.
- `CantDoReason::CashuEscrowNotLocked` (settle/cancel a never-locked escrow),
  `NotAllowedByStatus` (escrow already spent at the mint, §4B), plus the
  dispute-status reasons.

**No new protocol variant is required** — the `CashuPmSignature` /
`CashuSignatures` surface was landed in the 0.13.0 baseline exactly for this
track. The only new **daemon-side** capability is `sign_with_pm` in `CashuClient`
(if TA-1f has not already landed it).

---

## 4. `sign_with_pm` — the arbitrator signing primitive (CF-2 surface)

The one place the daemon uses its `P_M` key on **escrow** funds:

```rust
/// Produce Mostro's NUT-11 P2PK signature over every proof of the escrow
/// token, so the dispute winner can assemble a 2-of-3 SwapRequest
/// (P_M + winner). Returns one {secret, signature} per proof.
fn sign_with_pm(token: &Token, p_m_secret: &SecretKey)
    -> Result<Vec<CashuProofSignature>, Error>;
```

- It signs the escrow token's proofs only when a solver has ruled — never on
  the happy path, never unilaterally. The non-custodial guarantee holds:
  Mostro's one signature is worthless without the winner's second signature.
- The same primitive signs the **fee** token for TA-1f's redeem and Track C's
  refund; those are Mostro's own revenue, not escrow, and are out of this
  track's scope.
- The signature is delivered to the winner in `CashuPmSignature`; the winner (not
  Mostro) chooses the swap outputs and submits to the mint.

---

## 4B. Near-locktime — making the §4B obligation enforceable

Track A §4B: a `P_M` signature delivered after the seller has reclaimed via the
refund path is worthless. A log line and a priority flag do not prevent Mostro
from finalising a dispute and handing the buyer an unusable signature, so Track
D enforces the window at the three points it controls:

1. **Resolution SLA = `escrow_settlement_margin_days`** (Track B TB-1; default
   3). No second knob: the `FiatSent` guard already guarantees at least this
   much locktime remained when fiat moved, so it is the natural budget for a
   human resolution. `remaining = locktime.saturating_sub(now)` (unsigned,
   saturating — same arithmetic as Track B §4).
2. **On `AdminTakeDispute`** — alert, with a deadline. If
   `remaining < margin`, the solver's `AdminTookDispute` notification and the
   log carry the absolute locktime and the remaining time, and the dispute is
   priority-flagged. Informational, but now concrete.
3. **On `AdminSettle` (buyer wins)** — hard pre-sign guard, then finalise only
   on an observed spend:
   - `check_state` the escrow proofs **before** `sign_with_pm`. Any proof
     `SPENT` ⇒ **refuse** with `CantDo(NotAllowedByStatus)`, log at `error`
     with the mint answer, and notify the solver: the escrow has already moved
     (seller reclaim after locktime, or a buyer redeem the watcher has not seen
     yet). No signature is produced; the order does **not** change status. The
     solver's recovery path is to rule the way the funds actually went —
     `AdminCancel` if the seller reclaimed (which closes the order and refunds
     the fee), or wait one watcher tick if the buyer redeemed (the watcher then
     takes `SettledHoldInvoice/Dispute → Success` on its own evidence).
   - `UNSPENT` and `remaining == 0` ⇒ the seller *can* reclaim at any moment;
     signing is still the buyer's only chance, so sign and deliver, but the
     `CashuPmSignature` is accompanied by an explicit warning to the buyer
     ("swap immediately — the seller's refund path is open") and the log
     records the race. This residual window exists only if a dispute outlived
     the full margin; the §4B `FiatSent` guard and step 2 are what keep it rare.
   - Advance `Dispute → SettledByAdmin` with a conditional `UPDATE … WHERE
     status = Dispute`. **`CompletedByAdmin` is reached only by the release
     watcher** (Track B §5C) on `SPENT` observed before the locktime — never by
     the settle handler itself. A buyer who cannot use the signature therefore
     leaves the order in `SettledByAdmin`, visible to the solver, rather than
     in a terminal state that claims the buyer was paid.
   - **Re-delivery.** A repeated `AdminSettle` on a `SettledByAdmin` order
     re-signs (deterministic over the stored token) and re-sends the
     `CashuPmSignature` instead of being rejected — the recovery for a lost DM.
4. **On `AdminCancel` (seller wins)** — no time guard is needed: before the
   locktime the `P_M` signature lets the seller reclaim now; after it the
   seller reclaims alone. Sign, deliver, advance `Dispute → CanceledByAdmin`
   (conditional `UPDATE`), then `refund_cashu_fee`. `check_state` is still run
   first: `SPENT` before the locktime means the buyer redeemed (the seller
   released P2P during the dispute) — refuse with `NotAllowedByStatus` and let
   the watcher close it as `Success`; `SPENT` after the locktime means the
   seller already reclaimed — proceed with the status change and the refund,
   skip the (useless) signature.

---

## 5. Handlers — the Cashu branches

### 5A · `dispute_action` (Cashu branch)
Same identity rules as today. Status rule: today `dispute_action` admits only
`Active`/`FiatSent`; in Cashu mode it **also admits `SettledHoldInvoice`** —
the state Track B leaves an order in when the seller sent `Release` but the
buyer never received a usable signature (Track B §5C). Without this the buyer
has no recourse. In Cashu mode: advance to `Dispute`, no LND.

### 5B · `admin_take_dispute_action` (Cashu branch)
Assign the solver as today; additionally apply §4B(2): compute the remaining
locktime and, when below the margin, include the deadline in the solver's
notification and log, and priority-flag the dispute.

### 5C · `admin_settle_action` (buyer wins) / `admin_cancel_action` (seller wins)
In Cashu mode, replace the hold-invoice settle/cancel with §4B(3)/(4):
- `check_state` → refuse on an inconsistent spend state.
- `sign_with_pm(escrow_token)` → `CashuPmSignature` to the winner (buyer for
  settle, seller for cancel).
- Conditional status `UPDATE` to `SettledByAdmin` / `CanceledByAdmin`, publish
  the order event.
- **`admin_cancel` (seller wins) additionally calls `refund_cashu_fee`**
  (Track C §4) **after** its own status CAS succeeded — the helper's claim CAS
  is the backstop against a cooperative cancel landing at the same time (Track
  C §4.3(5)).
- Map a settle/cancel against a never-locked escrow to
  `CantDo(CashuEscrowNotLocked)`.

### 5D · Release watcher extension
Extend the Track B §5C watcher's selection to `status IN (SettledHoldInvoice,
SettledByAdmin)`; the `SPENT`-before-locktime arm maps `SettledHoldInvoice →
Success` and `SettledByAdmin → CompletedByAdmin` (and sends the `Rate`
requests in both cases). All other arms are unchanged.

### 5E · `dispatch_cashu` unblocks
Replace the `InvalidAction` arms for `Dispute`, `AdminTakeDispute`,
`AdminSettle`, `AdminCancel`, and route `AdminAddSolver` to
`handle_message_action_no_ln` (solver management touches no escrow/LND).

---

## 6. PR breakdown (atomic, backwards-compatible)

### TD-1 · `sign_with_pm` + `CashuClient` surface
Add `sign_with_pm` (NUT-11 P2PK) to `CashuClient`, unit-tested against the CF-3
mint (a `P_M`-signed proof + a winner signature satisfies the 2-of-3; a wrong key
does not; a `P_M` signature alone satisfies a 1-of-1 `P_M` token — the fee
case). Pure library; no daemon wiring. **Folds into TA-1f if TA-1f lands
first** — it is the same primitive; the tests above then live there.
*Depends on CF-2. Conflict surface: `cashu/mod.rs` (additive). Parallel with all.*

### TD-2 · `dispute` + `admin_take_dispute` Cashu branches + solver alert
Cashu branches for opening (incl. from `SettledHoldInvoice`) and taking a
dispute, plus the §4B(2) near-locktime solver alert with deadline. Unblock
`Dispute`, `AdminTakeDispute`, `AdminAddSolver`.
*Depends on CF-5, Track A, Track B TB-1 (the margin key). Conflict surface:
`dispute.rs`, `admin_take_dispute.rs`, `admin_add_solver.rs` (if touched),
`app.rs`.*

### TD-3 · `admin_settle` / `admin_cancel` Cashu branches + `P_M` delivery + fee refund
The §4B(3)/(4) guards, `CashuPmSignature` delivery to the winner, re-delivery on
repeat, the watcher extension (§5D), and the seller-wins call to
`refund_cashu_fee`. Unblock `AdminSettle`, `AdminCancel`. Completes dispute
resolution end-to-end.
*Depends on TD-1, TD-2, Track A, Track B TB-2 (the watcher), **Track C TC-1**
(the refund helper + its migration). Conflict surface: `admin_settle.rs`,
`admin_cancel.rs`, `scheduler.rs` (watcher arm, additive), `app.rs`.*

---

## 7. Issues table — sequential vs parallel

| ID | Title | Depends on | Parallel with | Conflict surface | Risk |
|----|-------|-----------|---------------|------------------|------|
| **TD-1** | `sign_with_pm` + CF-2 surface | CF-2 | everything (or folded into TA-1f) | `cashu/mod.rs` | Medium (crypto) |
| **TD-2** | `dispute`/`admin_take_dispute` Cashu + solver alert | CF-5, Track A, TB-1 | Track C; TB-2 | `dispute.rs`, `admin_take_dispute.rs`, `app.rs` | Medium |
| **TD-3** | `admin_settle`/`admin_cancel` + `P_M` delivery + watcher ext. + fee refund | TD-1, TD-2, Track A, **TB-2**, **TC-1** | — (last) | `admin_settle.rs`, `admin_cancel.rs`, `scheduler.rs`, `app.rs` | Medium-High (funds + revenue) |

**Sequencing.** TD-1 (library) can land first and in parallel with everything.
TD-2 needs only the TB-1 config key. **TD-3 is the integration point and lands
last:** it needs the signing primitive, the dispute-open path, Track B's
release watcher (to finalise `SettledByAdmin`) and Track C's `refund_cashu_fee`
(to refund on seller-wins). Track D is therefore parallel with Tracks B/C in
*code* (disjoint handler files) but **not in merge order** — TD-3 follows TB-2
and TC-1. If TD-3 must land before one of them, it carries the missing piece
itself (the watcher arm or the helper) and the other track consumes it.

---

## 8. Definition of Done

1. A disputed locked Cashu order can be resolved either way against the CF-3 mint:
   `AdminSettle` delivers a `P_M` signature the **buyer** uses to redeem, and the
   order reaches `CompletedByAdmin` only once the watcher observes the spend;
   `AdminCancel` delivers a `P_M` signature the **seller** uses to reclaim and
   the order reaches `CanceledByAdmin`.
2. Mostro's `P_M` signature over the escrow is produced **only** during dispute
   resolution, is worthless alone (the winner must add its own signature), and
   is delivered via `CashuPmSignature`; a repeated `AdminSettle` re-delivers it.
3. A seller-wins resolution refunds `2 * order.fee` to `P_S` through
   `refund_cashu_fee`, single-shot; a cooperative cancel racing the admin cancel
   yields one terminal status and one refund.
4. **Near-locktime is enforced, not just logged (§4B):** the solver is alerted
   with a deadline; `AdminSettle` against `SPENT` proofs is refused without
   producing a signature or changing status; a settle after the locktime
   delivers the signature with the explicit race warning; `SettledByAdmin`
   never becomes `CompletedByAdmin` without an observed spend before the
   locktime. Each case is asserted.
5. A settle/cancel against a never-locked escrow returns
   `CashuEscrowNotLocked`. `Dispute` is accepted from `SettledHoldInvoice` in
   Cashu mode and rejected there with Cashu disabled.
6. With Cashu disabled, behaviour is identical to `main`; existing tests pass
   unmodified. `fmt`/`clippy -D warnings`/`test` green.

---

## 9. Cross-track obligations satisfied / raised

| Obligation | Defined in | Track D does |
|------------|-----------|--------------|
| Dispute-near-locktime solver alert | Track A §4B | **Executed** (TD-2, with deadline) |
| Late `P_M` signature must not finalise a dispute the winner cannot complete | Track A §4B (made concrete here, §4B) | **Executed** (TD-3: pre-sign `check_state`, finalisation by observed spend) |
| Fee refund on dispute-resolved-for-seller | Track A §4A / §10; contract in Track C §4.3 | **Executed** (TD-3 calls `refund_cashu_fee`) |
| `Dispute` admitted from `SettledHoldInvoice` in Cashu mode | Track B §9 | **Executed** (TD-2) |
| Watcher finalises `SettledByAdmin → CompletedByAdmin` | Track B §9 | **Executed** (TD-3, §5D) |
| Every blocked admin/dispute action has an owner | CF-5 §6 matrix | **Executed** (TD-2/TD-3 unblock all dispute actions; `AddBondInvoice` stays permanently blocked) |

---

## 10. After Track D — the feature is complete

With Tracks A–D merged, a `[cashu] enabled = true` node can run a full trade
lifecycle — create, take, lock, release, cooperatively cancel, and resolve
disputes — entirely on ecash, with Mostro as a non-custodial coordinator that
signs only to arbitrate (and to hand back its own fee). The remaining open items
are the two scoped follow-ups Track A raised — the live fee-token redeem with its
**ecash revenue store** (which also unlocks the redeemed-fee refund path, Track
C §4.2) and the self-service-refund locktime refinement — neither of which
blocks a functioning Cashu marketplace.
