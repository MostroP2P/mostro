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
3. Mostro **advances the order state** (`Active → FiatSent → SettledHoldInvoice
   → Success`), where the final step is driven by a **daemon-verifiable fact**
   — the escrow proofs are observed **spent at the mint** before the locktime —
   not by the seller's word. It then makes the trade **rateable** and, because
   the fee was collected at lock (Track A §4A), takes **no** settlement action
   on the funds.

### In scope
- The Cashu branch of `fiat_sent_action` carrying the **§4B remaining-locktime
  guard** (`escrow_settlement_margin_days`, default 3), with its startup
  validation.
- The Cashu branch of `release_action`: the seller marks release, the daemon
  advances `FiatSent → SettledHoldInvoice` and notifies the buyer.
- The **release watcher** (§5C): a cashu-mode scheduler job that confirms the
  redeem at the mint and drives `SettledHoldInvoice → Success`, making the
  order rateable.
- Unblocking `FiatSent`, `Release`, and `RateUser` in `dispatch_cashu`.

### Out of scope (other tracks)
- **Cooperative cancel** → Track C. **Dispute resolution** → Track D (including
  the buyer's recourse when a seller "releases" but never delivers a usable
  signature — §9 raises it).
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
    M->>M: advance FiatSent -> SettledHoldInvoice
    M->>B: Released (seller released — redeem now)
    B->>Mint: SwapRequest {2-of-3 proofs, buyer_sig + seller_sig, buyer outputs}
    Mint-->>B: fresh ecash (buyer holds the funds)
    M->>Mint: check_state(escrow proof Ys)  [release watcher, periodic]
    Mint-->>M: SPENT (before locktime)
    M->>M: advance SettledHoldInvoice -> Success; make rateable
```

**State transitions Track B performs:**
`Active → FiatSent` (buyer), `FiatSent → SettledHoldInvoice` (seller release),
then `SettledHoldInvoice → Success` (watcher observes the redeem). This is the
exact status sequence of the Lightning flow (`release_action` already moves
`FiatSent → SettledHoldInvoice` and the payout moves it to `Success`), so the
public NIP-33 mapping and every status consumer are unchanged — but **no hold
invoice is settled and no payment is made**: the buyer redeems the token
itself, off Mostro's servers.

> **Why the seller's signature goes P2P, not through Mostro — and why Mostro
> must not relay it.** With `SIG_INPUTS` the seller's signature authorises a
> spend of the proofs with *any* outputs. Mostro already holds `P_M`; if it also
> held the seller's `P_S` signature it would have **two of three** and could swap
> the escrow to itself. Keeping the seller signature strictly seller→buyer
> (NIP-59 DM over the trade keys, the same channel the `release`/`cancel`
> messages already use) is what keeps Mostro non-custodial on the happy path.
> Consequently Mostro cannot *validate the signature*; what it can validate is
> the **outcome** — the proofs' NUT-07 state at the mint — which is how `Success`
> is reached (§5C).

---

## 3. What Track B consumes from Fundamentals + Track A

| Needs | From | Exact item |
|-------|------|------------|
| Mode gate | CF-1 | `Settings::is_cashu_enabled()`, `escrow_mode()` |
| Locktime floor / margin | CF-1 | `get_cashu().escrow_locktime_days`; **new** `escrow_settlement_margin_days` (default 3, §4) + `validate_cashu_settings` check |
| Locked escrow row | Track A | `Order.{cashu_escrow_token, cashu_escrow_locked_at}` populated, status `Active` |
| Token locktime read | CF-2 | parse the stored token's `locktime` (the guard compares it to now) |
| Proof state at the mint | CF-2 | `CashuClient::check_state(ys)` (NUT-07) — the release watcher's source of truth |
| Dispatch seam | CF-5 | `FiatSent`/`Release`/`RateUser` arms in `dispatch_cashu` |
| Notifications | existing | `enqueue_order_msg`, `update_order_event` |
| Cashu-mode scheduler slot | TA-1f / TA-3 | the cashu-gated job family in `scheduler.rs` (fee-redeem retry, lock monitor); the watcher is one more member |

Protocol (already on `main`, `mostro-core ≥ 0.14.0`): `Action::{FiatSent,
FiatSentOk, Release, Released, RateUser}` and the existing rating payloads —
**no new protocol variant is required**. The seller's Cashu release signature is
carried in the existing P2P release message shape (trade-key-signed NIP-59 DM);
Mostro validates the *state transition* and the *redeem outcome*, never the
swap itself.

---

## 4. The `escrow_settlement_margin_days` guard (§4B — Track B executes it)

Track A §4B defines the attack: a seller stalls the fiat phase until little
locktime remains, lets the buyer send fiat on day 13 of 15, goes silent, and
reclaims via the refund path on day 15 — keeping both fiat and sats without
failing a single protocol check. **Track B closes it at the `FiatSent` gate.**

- New `#[serde(default)]` key on `CashuSettings`: **`escrow_settlement_margin_days`,
  default 3**. Added by Track B (not needed during foundation).
- **Startup validation** (extend `validate_cashu_settings`, `src/config/util.rs`,
  which already rejects invalid `escrow_locktime_days`): when Cashu is enabled,
  require `1 <= escrow_settlement_margin_days < escrow_locktime_days`. Anything
  else is a **startup-fatal** config error — a margin at or above the locktime
  floor would reject *every* `FiatSent` on a minimum-locktime token, silently
  disabling the marketplace. Because the seller MAY set a longer locktime than
  the floor (Track A §4B), the guard always compares against the **token's
  actual** locktime, never the configured floor.
- In the Cashu branch of `fiat_sent_action`: read the stored escrow token's
  `locktime` and evaluate, in **unsigned seconds with saturating arithmetic**:

  ```text
  remaining = locktime.saturating_sub(now)           // 0 when already expired
  reject if remaining < escrow_settlement_margin_days * 86_400
  ```

  - token or `locktime` missing/unparseable ⇒ `CantDo(CashuEscrowNotLocked)`
    (an `Active` Cashu order always has both; their absence is a data bug, logged
    at `error`);
  - `remaining == 0` (locktime already passed — the seller can reclaim alone) or
    `remaining < margin` ⇒ reject. TB-1 ships this with the existing
    `CantDoReason::NotAllowedByStatus` so no protocol bump is needed; a dedicated
    `CantDoReason::CashuSettlementWindowClosed` is recorded as an **additive**
    `mostro-core` follow-up (clients already have to handle an unknown reason
    generically).

  Saturating subtraction means an expired locktime can never underflow into a
  huge "remaining" value that lets fiat through; the expired case collapses into
  the rejected branch by construction. Fiat can never be sent inside the danger
  window, so the seller cannot weaponise the locktime.

With the defaults (15-day floor, 3-day margin) the usable fiat-settlement window
on a minimum-locktime token is ≈12 days.

---

## 5. Handlers — the Cashu branches

### 5A · `fiat_sent_action` (Cashu branch)
Same identity/status checks as today (only the **buyer** may send fiat; order
must be `Active`). Then, in Cashu mode only: apply the §4 guard, advance
`Active → FiatSent`, publish the order event, and notify both parties
(`FiatSentOk`) — no LND, no hold-invoice interaction.

### 5B · `release_action` (Cashu branch)
The submitter must be the **seller** (same identity check as the Lightning
release); the order must be `FiatSent` (or `Dispute`, exactly as today). In
Cashu mode: **do not** settle a hold invoice (there is none); instead advance
`FiatSent → SettledHoldInvoice` with the **same conditional `UPDATE … WHERE
status IN (FiatSent, Dispute)`** the Lightning branch already uses
(`release.rs`), publish the order event, and notify the buyer with `Released`
("the seller released — redeem now"). Mostro MAY additionally relay the
seller's *message* as a reliability aid, but it **never carries or stores the
seller's Cashu signature** (§2 callout).

`SettledHoldInvoice` here means exactly what it means on Lightning: *the seller
has done their part; the funds have not yet reached the buyer.* The order is
**not** terminal and **not** rateable yet.

### 5C · Release watcher — `SettledHoldInvoice → Success` on observed redeem
A cashu-gated scheduler job (same family as the TA-1f fee-redeem retry and the
TA-3 lock monitor) that, every tick, selects orders with
`status = SettledHoldInvoice AND cashu_escrow_token IS NOT NULL`, computes the
escrow proofs' `Y = hash_to_curve(secret)` from the stored token, and calls
`CashuClient::check_state(ys)` (NUT-07):

- **All proofs `SPENT` and `now < locktime`** ⇒ the only spend path open before
  the locktime is the 2-of-3, and Mostro did not sign, so the spender is the
  buyer (with the seller's signature). Advance `SettledHoldInvoice → Success`
  with a conditional `UPDATE … WHERE status = SettledHoldInvoice`, publish the
  order event, notify both parties (`PurchaseCompleted`), and send the `Rate`
  requests — the trade is now rateable.
- **Any proof `UNSPENT`** ⇒ nothing to do; the buyer has not redeemed yet. The
  TA-3 monitor's locktime warnings to the buyer keep firing.
- **`SPENT` observed only after the locktime** ⇒ ambiguous (a late buyer redeem
  or the seller's §4B reclaim are indistinguishable from the mint's answer).
  Never auto-`Success`; log at `warn`, leave the order in `SettledHoldInvoice`
  for the dispute path (§9 → Track D).
- **Mint unreachable** ⇒ skip, retry next tick (the order stays eligible).

The watcher is the **only** path to `Success` in Cashu mode. A seller who sends
`Release` but never hands the buyer a usable signature therefore leaves the
order in `SettledHoldInvoice`, where the buyer can still open a dispute (Track D
admits `Dispute` from `SettledHoldInvoice` in Cashu mode — raised in §9) and a
solver can hand the buyer a `P_M` signature instead. Mostro never marks a
trade successful that the buyer could not complete.

### 5D · `dispatch_cashu` unblocks
Replace the `InvalidAction` arms for `FiatSent`, `Release`, and `RateUser`
(route through the existing `handle_message_action_no_ln`, whose branches now
carry the Cashu logic). `RateUser` keeps its existing `Success`-only gate, which
in Cashu mode is reachable only through the watcher. No other action changes.

---

## 6. PR breakdown (atomic, backwards-compatible)

### TB-1 · `escrow_settlement_margin_days` + `FiatSent` guard
Add the `CashuSettings` key, its `validate_cashu_settings` check, and the Cashu
branch of `fiat_sent_action` with the §4 remaining-locktime guard (saturating
arithmetic; expired ⇒ rejected). Unblock `FiatSent` in `dispatch_cashu`.
*Depends on CF-1, Track A. Conflict surface: `config/*`, `fiat_sent.rs`,
`app.rs` (one dispatch arm).*

### TB-2 · `release_action` Cashu branch + release watcher + rating
Add the Cashu branch of `release_action` (`FiatSent → SettledHoldInvoice`,
notify the buyer, no signature relay), the §5C release watcher
(`SettledHoldInvoice → Success` on observed spend before locktime), and unblock
`Release` + `RateUser`. Completes the happy path end-to-end with TB-1 and
Track A.
*Depends on CF-2 (`check_state`), CF-5, Track A, TB-1 (for a full e2e test).
Conflict surface: `release.rs`, `scheduler.rs` (additive, cashu-gated),
`rate_user.rs` (if touched), `app.rs` (two dispatch arms).*

---

## 7. Issues table — sequential vs parallel

| ID | Title | Depends on | Parallel with | Conflict surface | Risk |
|----|-------|-----------|---------------|------------------|------|
| **TB-1** | `escrow_settlement_margin_days` + `FiatSent` §4B guard | CF-1, Track A | Tracks C/D | `config/*`, `fiat_sent.rs` | Low-Medium |
| **TB-2** | `release_action` Cashu branch + release watcher + unblock `Release`/`RateUser` | CF-2, CF-5, Track A, TB-1 | Tracks C/D | `release.rs`, `scheduler.rs`, `app.rs` | Medium |

**Ordering.** Track B has **no merge-order dependency** on Tracks C or D in
either direction: nothing in B calls the Track C refund helper, and nothing in
C/D needs the watcher. "Parallel" here means *disjoint handler files*; the
shared touch points are the `dispatch_cashu` allow-list (edited one arm at a
time) and the cashu-gated job list in `scheduler.rs` (additive). Track D does
take one **behavioural** input from B — `Dispute` must be admitted from
`SettledHoldInvoice` in Cashu mode (§9) — which D implements whether it lands
before or after B.

---

## 8. Definition of Done

1. A locked Cashu order can be driven `Active → FiatSent → SettledHoldInvoice →
   Success` end-to-end against the CF-3 mint, with the buyer redeeming the 2-of-3
   token itself and the watcher observing the spend.
2. `FiatSent` is rejected inside the settlement-margin window (§4) **and** when
   the locktime has already passed, and accepted outside it; the exact
   `CantDoReason` is asserted. A margin `>= escrow_locktime_days` (or `0`) is
   rejected at startup by `validate_cashu_settings`.
3. `Release` alone never produces `Success`: with the proofs still `UNSPENT` the
   order stays in `SettledHoldInvoice` and is not rateable; a `SPENT` answer
   observed only after the locktime is logged and does **not** advance the
   order.
4. Every identity/status rejection path returns the correct reason and leaves the
   order unchanged (wrong sender, wrong status, guard tripped).
5. The trade becomes rateable only after the watcher reaches `Success`;
   `RateUser` works then and is rejected before.
6. With Cashu disabled, behaviour is identical to `main`; existing tests pass
   unmodified. `fmt`/`clippy -D warnings`/`test` green.

---

## 9. Cross-track obligations satisfied / raised

| Obligation | Defined in | Track B does |
|------------|-----------|--------------|
| `FiatSent` rejected when remaining locktime < `escrow_settlement_margin_days` (incl. already-expired) | Track A §4B | **Executed** (TB-1) |
| `RateUser` unblocked once terminal state reachable | CF-5 §6 | **Executed** (TB-2) |
| Buyer locktime warnings as expiry approaches | Track A §4B | Surfaced by TA-3 monitor; TB may add a nudge on `FiatSent` |
| `Dispute` admitted from `SettledHoldInvoice` in Cashu mode — the buyer's recourse when the seller releases but never delivers a usable signature (`dispute_action` today admits only `Active`/`FiatSent`) | **Raised here** (§5C) | **Executed by Track D** (TD-2, `dispute_action` Cashu branch) |
| `Success` is reached only on an observed mint spend before locktime, never on the seller's message alone | **Raised here** (§5C) | **Executed** (TB-2 watcher); Track D reuses the same watcher for `SettledByAdmin → CompletedByAdmin` |
