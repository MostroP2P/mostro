# Fix: Duplicate Dev Fee Payments (#620)

## Problem

The dev fee payment scheduler could pay the dev fee **twice** for the same order under certain conditions. This was observed on mainnet on February 24, 2026 — two separate Lightning payments of 68 sats were sent for order `bebb66a7-92e6-4cf2-9a56-2ff2a6247bc2`.

## Root Cause

A race condition in `job_process_dev_fee_payment()` in `src/scheduler.rs`:

1. Each call to `resolve_dev_fee_invoice()` resolves the LNURL address and gets a **fresh invoice** with a **new payment hash** (this is by design in LNURL-pay)
2. If the "real-hash cleanup" logic incorrectly reset an order's `dev_fee_paid` flag (e.g., because LND reported `Failed` for a payment that was still being indexed), the order would appear again in `find_unpaid_dev_fees()`
3. On the next cycle, a **new** LNURL invoice would be resolved (different hash), and a second payment would be sent

## Fix (Defense-in-Depth)

### Layer 1: Atomic Claim (Primary Defense)

Before resolving a new LNURL invoice, the scheduler now atomically claims the order using a SQL `UPDATE ... WHERE` statement:

```sql
UPDATE orders SET dev_fee_payment_hash = 'PENDING-{uuid}-{timestamp}'
WHERE id = ? AND dev_fee_paid = 0 AND dev_fee_payment_hash IS NULL
```

This ensures only one scheduler cycle can process an order at a time. If the claim returns `rows_affected() == 0`, the order was already claimed by another cycle and is skipped.

On failure (invoice resolution error/timeout), the claim is released:

```sql
UPDATE orders SET dev_fee_payment_hash = NULL
WHERE id = ? AND dev_fee_payment_hash = 'PENDING-{exact-marker}'
```

The release uses the exact marker value to prevent accidentally releasing a claim made by a different cycle.

### Layer 2: Query Filter (Secondary Defense)

The `find_unpaid_dev_fees()` query now excludes orders that have any `dev_fee_payment_hash` set:

```sql
SELECT * FROM orders
WHERE (status = 'settled-hold-invoice' OR status = 'success')
  AND dev_fee > 0
  AND dev_fee_paid = 0
  AND (dev_fee_payment_hash IS NULL OR dev_fee_payment_hash = '')
```

This means orders with a PENDING marker or a real payment hash (from a previous attempt) won't be picked up for processing again.

### Layer 3: Conservative Reset (Tertiary Defense)

The "real-hash cleanup" section no longer immediately resets orders when LND reports `Failed`. Instead, it logs a warning and skips the reset. This prevents premature resets that could cause the order to be re-processed.

The rationale is: it's better to have an unpaid dev fee that requires manual intervention than to pay it twice. The stale PENDING cleanup (with its 5-minute TTL) handles genuine crashes.

## Files Changed

- `src/scheduler.rs`: Added atomic claim, conservative reset logic
- `src/db.rs`: Updated `find_unpaid_dev_fees()` query filter

## Testing

The fix is primarily a logic change in the scheduler loop. Testing includes:
- Existing unit tests for `parse_pending_timestamp()` still pass
- The atomic claim uses standard SQLite atomic UPDATE semantics
- The release mechanism uses exact marker matching for safety
