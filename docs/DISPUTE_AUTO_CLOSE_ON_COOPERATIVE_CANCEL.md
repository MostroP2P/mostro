# Dispute Auto-Close on Cooperative Cancellation

## Overview

When both parties in an active trade agree to a cooperative cancellation, any
open dispute associated with that order is now **automatically closed** by
Mostrod. Previously the dispute remained in `initiated` or `in-progress` state
even after the order was successfully canceled, creating "ghost" disputes
visible to admins.

## Problem

The cooperative cancellation flow correctly handled the Lightning side (canceling
the hold invoice and returning sats to the seller), but the dispute state machine
did not account for this exit path:

1. Buyer and seller are in an active order
2. One party opens a dispute (status: `initiated`, kind 38386 event published)
3. Before an admin takes the dispute, both users agree to cooperatively cancel
4. The cancellation succeeds — sats are returned to the seller ✅
5. **Bug:** The dispute still appeared as active in admin tools (Mostrix, etc.)

## Solution

The fix is in `src/app/cancel.rs`, specifically in the
`cancel_cooperative_execution_step_2` function. After the cooperative
cancellation completes successfully (hold invoice canceled, order status set to
`CooperativelyCanceled`, both parties notified), the code now:

1. **Queries for an active dispute** on the order via `find_dispute_by_order_id`
2. **Updates the dispute status** to `SellerRefunded` (consistent with the
   admin cancel flow, since sats are returned to the seller)
3. **Publishes an updated NIP-33 dispute event** (kind 38386) to Nostr with
   the new status, so admin clients see the dispute as resolved

### Code Flow

```
cancel_action()
  └─ cancel_active_order()
       └─ cancel_cooperative_execution_step_2()
            ├─ Cancel hold invoice (return sats to seller)
            ├─ Set order status to CooperativelyCanceled
            ├─ Publish updated order event (kind 38383)
            ├─ Notify both parties
            └─ [NEW] Close associated dispute if any:
                 ├─ Update dispute status to SellerRefunded in DB
                 └─ Publish updated dispute event (kind 38386)
```

### Why `SellerRefunded`?

The `DisputeStatus::SellerRefunded` status is used because in a cooperative
cancellation the hold invoice is canceled, which returns the locked sats to
the seller's wallet. This is semantically identical to what happens when an
admin cancels a disputed order — the seller gets refunded. Using the same
status ensures consistency in admin tools and event consumers.

### Error Handling

The dispute closure is **best-effort** — if the dispute update or event
publication fails, the error is logged but does not prevent the cooperative
cancellation from completing. The primary operation (canceling the order and
returning funds) takes priority.

## Related

- Issue: [#577](https://github.com/MostroP2P/mostro/issues/577)
- Dispute events: NIP-33 replaceable events, kind 38386
- Order events: NIP-33 replaceable events, kind 38383
- Admin cancel flow: `src/app/admin_cancel.rs` (reference implementation for
  dispute status updates)
