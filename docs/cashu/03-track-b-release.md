# Cashu Escrow — Track B: Release (Happy Path)

**Status:** Draft for review · **Target:** `main` (**requires `mostro-core ≥ 0.14.0`**) ·
**Depends on:** Fundamentals **CF-1, CF-2, CF-5** + **Track A** (the escrow must be
locked before it can be released) · **Feature flag:** `[cashu].enabled`

Track B is the **happy-path settlement** of a Cashu trade: the buyer confirms
fiat sent, the seller releases, and the buyer redeems the locked 2-of-3 token
with **two signatures** (its own + the seller's). It is "box 3" of the sequence
diagram in [`../CASHU_ESCROW_ARCHITECTURE.md`](../CASHU_ESCROW_ARCHITECTURE.md).

This document assumes Fundamentals and Track A are merged. It only adds behaviour
*inside the Cashu branch*; the Lightning path is never changed.

---

## 1. Goal and scope

### Goal
Complete a locked Cashu trade without Mostro ever touching the funds:
1. The **buyer** signals `FiatSent` (gated by the remaining-locktime guard, §4B).
2. The **seller** signals `Release`, and delivers its **Cashu release signature**
   directly to the buyer (P2P NIP-59 DM) so the buyer can build a valid 2-of-3
   `SwapRequest` and redeem the ecash **itself**.
3. Mostro only **advances the order state** (`Active → FiatSent → Success`),
   makes the trade **rateable**, and — because the fee was collected at lock
   (Track A §4A) — takes **no** settlement action on the funds.

### In scope
- The Cashu branch of `fiat_sent_action` carrying the **§4B remaining-locktime
  guard** (`escrow_settlement_margin_days`, default 3).
- The Cashu branch of `release_action`: the seller marks release, the daemon
  advances state and relays/acknowledges the seller's release signature to the
  buyer, and the order becomes rateable.
- Unblocking `FiatSent`, `Release`, and `RateUser` in `dispatch_cashu`.

### Out of scope (other tracks)
- **Cooperative cancel** → Track C. **Dispute resolution** → Track D.
- The **live fee redeem** (Track A TA-1f follow-up) — orthogonal to release.
- Any change to the Lightning release/settle path.

---

## 2. Where Track B sits — flow and state transitions

```mermaid
sequenceDiagram
    participant B as Buyer
    participant M as Mostro (cashu mode)
    participant S as Seller
    participant Mint as Cashu Mint

    Note over B,S: Escrow already locked (Track A) — order is Active
    B->>M: FiatSent
    M->>M: guard: remaining locktime >= escrow_settlement_margin_days
    M->>S: FiatSentOk (buyer paid — release when ready)
    S->>B: Release signature (NIP-59 DM, seller signs SIG_INPUTS)
    S->>M: Release
    M->>M: advance Active/FiatSent -> Success; make rateable
    B->>Mint: SwapRequest {2-of-3 proofs, buyer_sig + seller_sig, buyer outputs}
    Mint-->>B: fresh ecash (buyer holds the funds)
```

**State transitions Track B performs:**
`Active → FiatSent` (buyer), then `FiatSent → Success` (seller release). Both are
Cashu analogues of the Lightning flow, but **no hold invoice is settled** — the
buyer redeems the token itself, off Mostro's servers.

> **Why the seller's signature goes P2P, not through Mostro.** On the happy path
> Mostro is neither a signer nor a required relay: the seller signs the release
> and hands it to the buyer over a NIP-59 DM using the trade keys, exactly as the
> `release`/`cancel` messages already travel. Mostro only needs the **state**
> update. (Mostro MAY still relay a copy for reliability, but the trust model
> does not depend on it.)

---

## 3. What Track B consumes from Fundamentals + Track A

| Needs | From | Exact item |
|-------|------|------------|
| Mode gate | CF-1 | `Settings::is_cashu_enabled()`, `escrow_mode()` |
| Locktime floor / margin | CF-1 | `get_cashu().escrow_locktime_days`; **new** `escrow_settlement_margin_days` (default 3, §4B) |
| Locked escrow row | Track A | `Order.{cashu_escrow_token, cashu_escrow_locked_at}` populated, status `Active` |
| Token locktime read | CF-2 | parse the stored token's `locktime` (the guard compares it to now) |
| Dispatch seam | CF-5 | `FiatSent`/`Release`/`RateUser` arms in `dispatch_cashu` |
| Notifications | existing | `enqueue_order_msg`, `update_order_event` |

Protocol (already on `main`, `mostro-core ≥ 0.14.0`): `Action::{FiatSent,
FiatSentOk, Release, Released, RateUser}` and the existing rating payloads —
**no new protocol variant is required**. The seller's Cashu release signature is
carried in the existing P2P release message shape (trade-key-signed NIP-59 DM);
Mostro validates the *state transition*, not the swap.

---

## 4. The `escrow_settlement_margin_days` guard (§4B — Track B executes it)

Track A §4B defines the attack: a seller stalls the fiat phase until little
locktime remains, lets the buyer send fiat on day 13 of 15, goes silent, and
reclaims via the refund path on day 15 — keeping both fiat and sats without
failing a single protocol check. **Track B closes it at the `FiatSent` gate.**

- New `#[serde(default)]` key on `CashuSettings`: **`escrow_settlement_margin_days`,
  default 3**. Added by Track B (not needed during foundation).
- In the Cashu branch of `fiat_sent_action`: read the stored escrow token's
  `locktime`; reject `FiatSent` with a clear `CantDo` (e.g.
  `CashuEscrowNotLocked` if the token/locktime is missing, else a
  settlement-window reason) when
  `locktime - now < escrow_settlement_margin_days`. Fiat can never be sent inside
  the danger window, so the seller cannot weaponise the locktime.

The margin must be comfortably below the locktime floor (`escrow_locktime_days`,
default 15) so a normal trade has ample settlement room; the difference (≈12
days) is the usable fiat-settlement window.

---

## 5. Handlers — the Cashu branches

### 5A · `fiat_sent_action` (Cashu branch)
Same identity/status checks as today (only the **buyer** may send fiat; order
must be `Active`). Then, in Cashu mode only: apply the §4B guard, advance
`Active → FiatSent`, publish the order event, and notify both parties
(`FiatSentOk`) — no LND, no hold-invoice interaction.

### 5B · `release_action` (Cashu branch)
The submitter must be the **seller** (same identity check as the Lightning
release). In Cashu mode: **do not** settle a hold invoice (there is none);
instead advance `FiatSent → Success`, publish the order event, acknowledge the
seller's release signature to the buyer (relay/ack the P2P DM), and make the
trade rateable (`RateUser` becomes valid). The fee was already collected at lock
(Track A §4A), so release moves **no** funds.

### 5C · `dispatch_cashu` unblocks
Replace the `InvalidAction` arms for `FiatSent`, `Release`, and `RateUser`
(route through the existing `handle_message_action_no_ln`, whose branches now
carry the Cashu logic). No other action changes.

---

## 6. PR breakdown (atomic, backwards-compatible)

### TB-1 · `escrow_settlement_margin_days` + `FiatSent` guard
Add the `CashuSettings` key and the Cashu branch of `fiat_sent_action` with the
§4B remaining-locktime guard. Unblock `FiatSent` in `dispatch_cashu`.
*Depends on CF-1, Track A. Conflict surface: `config/*`, `fiat_sent.rs`,
`app.rs` (one dispatch arm).*

### TB-2 · `release_action` Cashu branch + rating
Add the Cashu branch of `release_action` (advance state, ack the seller
signature, make rateable) and unblock `Release` + `RateUser`. Completes the
happy path end-to-end with TB-1 and Track A.
*Depends on CF-5, Track A, TB-1 (for a full e2e test). Conflict surface:
`release.rs`, `rate_user.rs` (if touched), `app.rs` (two dispatch arms).*

---

## 7. Issues table — sequential vs parallel

| ID | Title | Depends on | Parallel with | Conflict surface | Risk |
|----|-------|-----------|---------------|------------------|------|
| **TB-1** | `escrow_settlement_margin_days` + `FiatSent` §4B guard | CF-1, Track A | Tracks C/D | `config/*`, `fiat_sent.rs` | Low-Medium |
| **TB-2** | `release_action` Cashu branch + unblock `Release`/`RateUser` | CF-5, Track A, TB-1 | Tracks C/D | `release.rs`, `app.rs` | Medium |

All of Track B is parallel with Tracks C/D (disjoint handler files); the shared
touch point is the `dispatch_cashu` allow-list, edited one arm at a time.

---

## 8. Definition of Done

1. A locked Cashu order can be driven `Active → FiatSent → Success` end-to-end
   against the CF-3 mint, with the buyer redeeming the 2-of-3 token itself.
2. `FiatSent` is rejected inside the settlement-margin window (§4B), and accepted
   outside it; the exact `CantDoReason` is asserted.
3. Every identity/status rejection path returns the correct reason and leaves the
   order unchanged (wrong sender, wrong status, guard tripped).
4. The trade becomes rateable only after release; `RateUser` works.
5. With Cashu disabled, behaviour is identical to `main`; existing tests pass
   unmodified. `fmt`/`clippy -D warnings`/`test` green.

---

## 9. Cross-track obligations satisfied / raised

| Obligation | Defined in | Track B does |
|------------|-----------|--------------|
| `FiatSent` rejected when remaining locktime < `escrow_settlement_margin_days` | Track A §4B | **Executed** (TB-1) |
| `RateUser` unblocked once terminal state reachable | CF-5 §6 | **Executed** (TB-2) |
| Buyer locktime warnings as expiry approaches | Track A §4B | Surfaced by TA-3 monitor; TB may add a nudge on `FiatSent` |
