# Fix: Duplicate Dev Fee Payments (#620)

## Problem

The dev fee payment scheduler could pay the dev fee **twice** for the same order. Observed on mainnet (Feb 24, 2026): order `bebb66a7` received two payments of 68 sats each, 122 seconds apart, with different payment hashes.

## Root Cause

Each call to `resolve_dev_fee_invoice()` resolves the LNURL address and gets a **fresh invoice** with a **new payment hash** (by LNURL-pay design). If the scheduler processed the same order twice — due to premature resets, crash recovery, or race conditions — it would resolve two different invoices and pay both.

The most likely trigger: the "real-hash cleanup" section reset an order's `dev_fee_paid` flag when LND reported `Failed` for a payment that was still being indexed, making the order eligible for reprocessing.

## Fix: Defense-in-Depth (4 Layers)

### Layer 1: Idempotency Check (Primary Defense)

**File:** `src/scheduler.rs` — new section before `find_unpaid_dev_fees()`

Before resolving any new LNURL invoices, the scheduler queries orders that have a real payment hash but `dev_fee_paid = 0` (partial success state). For each:

| LND Status | Action |
|---|---|
| `Succeeded` | Mark as paid, publish audit event — **no new invoice** |
| `Failed` | Clear hash — order becomes eligible for fresh LNURL resolution |
| `InFlight` | Skip — payment may still complete, **no new invoice** |
| `Unknown` | Skip — err on side of caution, **no new invoice** |

This ensures we **never resolve a second LNURL invoice while an existing payment is pending or succeeded**. The hash acts as an idempotency key.

### Layer 2: Atomic Claim Guard (Secondary Defense)

**File:** `src/scheduler.rs` — within the `find_unpaid_dev_fees()` processing loop

Before resolving a new LNURL invoice for orders with no existing hash, atomically claim the order:

```sql
UPDATE orders SET dev_fee_payment_hash = 'PENDING-{uuid}-{timestamp}'
WHERE id = ? AND dev_fee_paid = 0
  AND (dev_fee_payment_hash IS NULL OR dev_fee_payment_hash = '')
```

If `rows_affected() == 0`, another cycle already claimed it — skip. On failure (resolution error/timeout), the claim is released using exact marker matching.

### Layer 3: Query Filter (Tertiary Defense)

**File:** `src/db.rs` — `find_unpaid_dev_fees()`

The query now excludes orders with any `dev_fee_payment_hash`:

```sql
AND (dev_fee_payment_hash IS NULL OR dev_fee_payment_hash = '')
```

Orders with a PENDING marker or real hash are never picked up by `find_unpaid_dev_fees()`.

### Layer 4: Conservative Reset (Safety Net)

**File:** `src/scheduler.rs` — real-hash cleanup section

The "real-hash cleanup" no longer resets orders when LND reports `Failed`. Instead it logs a warning. This prevents premature resets caused by LND indexing delays.

**Principle:** Better an unpaid dev fee (manual reconciliation) than a duplicate payment (unrecoverable loss).

## Payment Lifecycle

```text
Order created (dev_fee_paid=0, hash=NULL)
    │
    ▼
find_unpaid_dev_fees() picks it up
    │
    ▼
Atomic claim: hash = "PENDING-{uuid}-{ts}"
    │
    ▼
resolve_dev_fee_invoice() → new LNURL invoice
    │
    ▼
Store real hash: hash = "abc123...", dev_fee_paid = true
    │
    ▼
send_dev_fee_payment() → LND pays the invoice
    │
    ├─ Success → publish audit event, done ✅
    ├─ Failure → keep hash, dev_fee_paid = false
    │            (idempotency check on next cycle will verify)
    └─ Timeout → check LND status
                 ├─ Succeeded → done ✅
                 ├─ InFlight → keep hash, wait
                 ├─ Failed → clear hash, retry next cycle
                 └─ Unknown → keep hash, wait
```

### Crash Recovery Scenarios

| Crash Point | State After Crash | Recovery |
|---|---|---|
| After claim, before resolution | `hash = "PENDING-..."` | Stale cleanup (5min TTL) clears it |
| After storing real hash, before payment | `hash = "abc123"`, `paid = true` | Real-hash cleanup verifies with LND |
| After payment, before DB update | `hash = "abc123"`, `paid = true` | Already correct state |
| After payment, `paid` stuck at `false` | `hash = "abc123"`, `paid = false` | **Idempotency check** finds hash, verifies with LND, marks as paid |

The last scenario is what caused the original bug. Previously, the order would be picked up by `find_unpaid_dev_fees()` and a new LNURL invoice would be resolved. Now, the idempotency check (Layer 1) catches it first.

## Files Changed

| File | Changes |
|---|---|
| `src/scheduler.rs` | Idempotency check (Option A), atomic claim (Option B), conservative reset |
| `src/db.rs` | Updated `find_unpaid_dev_fees()` query filter |
| `docs/FIX_DUPLICATE_DEV_FEE.md` | This documentation |

## Testing

- Existing unit tests for `parse_pending_timestamp()` pass unchanged
- New unit tests for `find_unpaid_dev_fees_query_filter` verify the query excludes orders with existing hashes
- Atomic claim uses standard SQLite atomic UPDATE semantics
- Release mechanism uses exact marker matching for safety
