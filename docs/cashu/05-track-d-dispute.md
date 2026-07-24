# Cashu Escrow — Track D: Dispute Resolution (`P_M` signs)

**Status:** Draft for review · **Target:** `main` (**requires `mostro-core ≥ 0.14.0`**) ·
**Depends on:** Fundamentals **CF-1, CF-2, CF-5** + **Track A** (the escrow must be
locked before it can be disputed) · **Feature flag:** `[cashu].enabled`

Track D is the **only** track where Mostro's arbitrator key `P_M` produces a
signature. When a locked Cashu trade cannot be resolved cooperatively, a solver
decides the outcome and Mostro hands the **winner** its `P_M` signature so the
winner can complete a 2-of-3 swap and take the funds. This closes the trust model:
the 2-of-3 exists precisely so that a single honest arbitrator can break a
buyer↔seller deadlock without ever holding the funds.

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
     **refunds the seller the fee** (Track A §4A).
3. Mostro advances the order to its terminal dispute state and makes the outcome
   auditable.

### In scope
- `sign_with_pm` — the `P_M` signing primitive (NUT-11 P2PK) in `CashuClient`.
- The Cashu branches of `dispute_action`, `admin_take_dispute_action`,
  `admin_settle_action`, `admin_cancel_action`, delivering `CashuPmSignature`.
- Unblocking `Dispute`, `AdminTakeDispute`, `AdminSettle`, `AdminCancel`,
  `AdminAddSolver` in `dispatch_cashu`.
- The **dispute-near-locktime solver alert** (Track A §4B).
- The **fee refund** on a seller-wins resolution (Track A §4A).

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
    Note over M,Solver: solver alerted if remaining locktime < resolution SLA (§4B)
    Solver->>M: AdminSettle (buyer wins) | AdminCancel (seller wins)
    M->>M: sign_with_pm(escrow proofs)
    M->>W: CashuPmSignature { per-proof {secret, signature} }
    W->>Mint: SwapRequest {2-of-3 proofs, P_M sig + own sig, own outputs}
    Mint-->>W: funds to the winner
    Note over M: if seller wins, refund 2*order.fee to P_S (§4A)
```

**State transitions Track D performs:** `Dispute` opens the dispute
(`Active/FiatSent → Dispute`); `AdminSettle`/`AdminCancel` drive it to the
terminal `SettledByAdmin`/`CanceledByAdmin` (or the existing dispute-terminal
statuses), mirroring the Lightning admin flow but with **no hold-invoice
settle/cancel** — Mostro emits a signature instead.

---

## 3. What Track D consumes from Fundamentals + Track A

| Needs | From | Exact item |
|-------|------|------------|
| Mode gate | CF-1 | `Settings::is_cashu_enabled()` |
| Locked escrow row | Track A | `Order.{cashu_escrow_token}` populated |
| `P_M` signing | CF-2 (this track adds it) | `CashuClient::sign_with_pm(token \| proofs) -> Vec<CashuProofSignature>` |
| Winner pubkey | existing | `order.get_buyer_pubkey()` / `get_seller_pubkey()` |
| Fee refund (seller wins) | Track A §4A | `2 * order.fee` to `P_S`; shared with Track C's refund |
| Solver management | existing | `admin_add_solver`, `admin_take_dispute` |
| Dispatch seam | CF-5 | `Dispute`/`AdminTakeDispute`/`AdminSettle`/`AdminCancel`/`AdminAddSolver` arms |

Protocol (already on `main`, `mostro-core ≥ 0.14.0`, frozen):
- `Action::CashuPmSignature` (Mostro → winner) carrying
  `Payload::CashuSignatures(Vec<CashuProofSignature>)`, where
  `CashuProofSignature = { secret, signature }` — **one entry per escrow proof**.
- `Action::{Dispute, AdminTakeDispute, AdminSettle, AdminCancel, AdminAddSolver}`
  and the dispute payloads.
- `CantDoReason::CashuEscrowNotLocked` (settle/cancel a never-locked escrow),
  plus the dispute-status reasons.

**No new protocol variant is required** — the `CashuPmSignature` /
`CashuSignatures` surface was landed in the 0.13.0 baseline exactly for this
track. The only new **daemon-side** capability is `sign_with_pm` in `CashuClient`.

---

## 4. `sign_with_pm` — the arbitrator signing primitive (CF-2 surface)

The one place the daemon uses its `P_M` key on funds-bearing material:

```rust
/// Produce Mostro's NUT-11 P2PK signature over every proof of the escrow
/// token, so the dispute winner can assemble a 2-of-3 SwapRequest
/// (P_M + winner). Returns one {secret, signature} per proof.
fn sign_with_pm(token: &Token, p_m_secret: &SecretKey)
    -> Result<Vec<CashuProofSignature>, Error>;
```

- It signs **only** the escrow token's proofs, and only when a solver has ruled —
  never on the happy path, never unilaterally. The non-custodial guarantee holds:
  Mostro's one signature is worthless without the winner's second signature.
- The signature is delivered to the winner in `CashuPmSignature`; the winner (not
  Mostro) chooses the swap outputs and submits to the mint.

---

## 5. Handlers — the Cashu branches

### 5A · `dispute_action` (Cashu branch)
Same identity/status rules as today (a party to a locked/active order opens the
dispute). In Cashu mode: advance to `Dispute`, no LND.

### 5B · `admin_take_dispute_action` (Cashu branch)
Assign the solver as today; additionally, **alert the solver** when the escrow's
remaining locktime is below the resolution SLA (§4B) — a late `P_M` signature is
worthless once the seller can reclaim via the refund path. Log + priority-flag.

### 5C · `admin_settle_action` (buyer wins) / `admin_cancel_action` (seller wins)
In Cashu mode, replace the hold-invoice settle/cancel with:
- `sign_with_pm(escrow_token)` → `CashuPmSignature` to the winner (buyer for
  settle, seller for cancel).
- Advance to the terminal dispute status, publish the order event.
- **`admin_cancel` (seller wins) additionally refunds the fee** (`2 * order.fee`
  to `P_S`, §4A) — single-shot, shared with Track C's refund bookkeeping.
- Map a settle/cancel against a never-locked escrow to
  `CantDo(CashuEscrowNotLocked)`.

### 5D · `dispatch_cashu` unblocks
Replace the `InvalidAction` arms for `Dispute`, `AdminTakeDispute`,
`AdminSettle`, `AdminCancel`, and route `AdminAddSolver` to
`handle_message_action_no_ln` (solver management touches no escrow/LND).

---

## 6. PR breakdown (atomic, backwards-compatible)

### TD-1 · `sign_with_pm` + `CashuClient` surface
Add `sign_with_pm` (NUT-11 P2PK) to `CashuClient`, unit-tested against the CF-3
mint (a `P_M`-signed proof + a winner signature satisfies the 2-of-3; a wrong key
does not). Pure library; no daemon wiring.
*Depends on CF-2. Conflict surface: `cashu/mod.rs` (additive). Parallel with all.*

### TD-2 · `dispute` + `admin_take_dispute` Cashu branches + solver alert
Cashu branches for opening and taking a dispute, plus the §4B near-locktime
solver alert. Unblock `Dispute`, `AdminTakeDispute`, `AdminAddSolver`.
*Depends on CF-5, Track A. Conflict surface: `dispute.rs`,
`admin_take_dispute.rs`, `admin_add_solver.rs` (if touched), `app.rs`.*

### TD-3 · `admin_settle` / `admin_cancel` Cashu branches + `P_M` delivery + fee refund
Deliver `CashuPmSignature` to the winner; seller-wins path refunds the fee.
Unblock `AdminSettle`, `AdminCancel`. Completes dispute resolution end-to-end.
*Depends on TD-1, TD-2, Track A (+ TA-1f/Track C for the shared refund).
Conflict surface: `admin_settle.rs`, `admin_cancel.rs`, `app.rs`.*

---

## 7. Issues table — sequential vs parallel

| ID | Title | Depends on | Parallel with | Conflict surface | Risk |
|----|-------|-----------|---------------|------------------|------|
| **TD-1** | `sign_with_pm` + CF-2 surface | CF-2 | everything | `cashu/mod.rs` | Medium (crypto) |
| **TD-2** | `dispute`/`admin_take_dispute` Cashu + solver alert | CF-5, Track A | Tracks B/C | `dispute.rs`, `admin_take_dispute.rs`, `app.rs` | Medium |
| **TD-3** | `admin_settle`/`admin_cancel` + `P_M` delivery + fee refund | TD-1, TD-2, Track A | Tracks B/C | `admin_settle.rs`, `admin_cancel.rs`, `app.rs` | Medium-High (funds + revenue) |

**Sequencing:** TD-1 (library) can land first and in parallel with everything;
TD-2 and TD-3 are the daemon wiring, TD-3 last (it needs the signing primitive and
the dispute-open path). All of Track D is parallel with Tracks B/C.

---

## 8. Definition of Done

1. A disputed locked Cashu order can be resolved either way against the CF-3 mint:
   `AdminSettle` delivers a `P_M` signature the **buyer** uses to redeem;
   `AdminCancel` delivers a `P_M` signature the **seller** uses to reclaim.
2. Mostro's `P_M` signature is produced **only** during dispute resolution, is
   worthless alone (the winner must add its own signature), and is delivered via
   `CashuPmSignature`.
3. A seller-wins resolution refunds `2 * order.fee` to `P_S`, single-shot.
4. The solver is alerted when remaining locktime is below the resolution SLA
   (§4B). A settle/cancel against a never-locked escrow returns
   `CashuEscrowNotLocked`.
5. With Cashu disabled, behaviour is identical to `main`; existing tests pass
   unmodified. `fmt`/`clippy -D warnings`/`test` green.

---

## 9. Cross-track obligations satisfied / raised

| Obligation | Defined in | Track D does |
|------------|-----------|--------------|
| Dispute-near-locktime solver alert | Track A §4B | **Executed** (TD-2) |
| Fee refund on dispute-resolved-for-seller | Track A §4A / §10 | **Executed** (TD-3, shared with Track C) |
| Every blocked admin/dispute action has an owner | CF-5 §6 matrix | **Executed** (TD-2/TD-3 unblock all dispute actions; `AddBondInvoice` stays permanently blocked) |

---

## 10. After Track D — the feature is complete

With Tracks A–D merged, a `[cashu] enabled = true` node can run a full trade
lifecycle — create, take, lock, release, cooperatively cancel, and resolve
disputes — entirely on ecash, with Mostro as a non-custodial coordinator that
signs only to arbitrate. The remaining open items are the two scoped follow-ups
Track A raised (the live fee-token redeem / ecash revenue store, and the
self-service-refund locktime refinement), neither of which blocks a functioning
Cashu marketplace.
