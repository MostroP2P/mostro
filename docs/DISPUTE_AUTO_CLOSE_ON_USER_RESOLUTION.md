# Dispute Auto-Close on User Resolution

## Overview

When users resolve a disputed order themselves — without admin intervention —
Mostrod now **automatically closes the dispute**. This happens in two scenarios:

| Scenario | Order Outcome | Dispute Status | Sats Go To |
|----------|---------------|----------------|------------|
| Cooperative Cancellation | `CooperativelyCanceled` | `SellerRefunded` | Seller |
| Seller Release | `Success` | `Settled` | Buyer |

Previously, disputes remained in `initiated` or `in-progress` state even after
the order was resolved by the users, creating "ghost" disputes visible to admins.

## Problem

Both the cooperative cancellation and release flows correctly handled the
Lightning side, but the dispute state machine did not account for these exit
paths:

1. Buyer and seller are in an active order
2. One party opens a dispute (status: `initiated`, kind 38386 event published)
3. Before an admin takes the dispute, the users resolve it themselves:
   - **Either** both agree to cooperatively cancel, **or**
   - The seller decides to release the funds anyway
4. The order completes successfully ✅
5. **Bug:** The dispute still appeared as active in admin tools (Mostrix, etc.)

## Solution

### Case 1: Cooperative Cancellation

**File:** `src/app/cancel.rs` — `cancel_cooperative_execution_step_2()`

After the cooperative cancellation completes (hold invoice canceled, sats
returned to seller):

1. Query for an active dispute via `find_dispute_by_order_id`
2. Update dispute status to `SellerRefunded`
3. Publish updated NIP-33 dispute event (kind 38386)

**Why `SellerRefunded`?** The hold invoice is canceled, returning sats to the
seller. This is semantically identical to an admin cancel.

```text
cancel_action()
  └─ cancel_active_order()
       └─ cancel_cooperative_execution_step_2()
            ├─ Cancel hold invoice (return sats to seller)
            ├─ Set order status to CooperativelyCanceled
            ├─ Notify both parties
            └─ Close dispute → status: SellerRefunded
```

### Case 2: Seller Release

**File:** `src/app/release.rs` — `release_action()`

After the seller releases funds (hold invoice settled, payment initiated to
buyer):

1. Query for an active dispute via `find_dispute_by_order_id`
2. Update dispute status to `Settled`
3. Publish updated NIP-33 dispute event (kind 38386)

**Why `Settled`?** The seller voluntarily released the funds, which will go to
the buyer. This is semantically identical to an admin settle.

```text
release_action()
  ├─ Settle hold invoice
  ├─ Set order status to SettledHoldInvoice
  ├─ Close dispute → status: Settled
  ├─ Notify both parties
  └─ Initiate payment to buyer
```

## Dispute Status Reference

| Status | Meaning | Triggered By |
|--------|---------|--------------|
| `Initiated` | Dispute opened, waiting for admin | User opens dispute |
| `InProgress` | Admin took the dispute | Admin action |
| `SellerRefunded` | Resolved, sats returned to seller | Admin cancel or cooperative cancel |
| `Settled` | Resolved, sats sent to buyer | Admin settle or seller release |

## Error Handling

Dispute closure is **best-effort** in both cases. If the dispute update or
event publication fails, the error is logged but does not prevent the primary
operation from completing. The order resolution (cancel or release) takes
priority over dispute bookkeeping.

## Related

- Cooperative cancel fix: [#577](https://github.com/MostroP2P/mostro/issues/577), [#578](https://github.com/MostroP2P/mostro/pull/578)
- Seller release fix: [#605](https://github.com/MostroP2P/mostro/issues/605), [#606](https://github.com/MostroP2P/mostro/pull/606)
- Dispute events: NIP-33 replaceable events, kind 38386
- Order events: NIP-33 replaceable events, kind 38383
