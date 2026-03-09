# Dev Fee Timeout Safety — Duplicate Payment Prevention

## Overview

When a dev fee Lightning payment times out, Mostrod queries the LN node for
the actual payment status before deciding whether to reset and retry. This
prevents duplicate payments caused by race conditions between timeouts and
in-flight payments.

## Problem

Originally, when the 50-second timeout expired during a dev fee payment, the
code unconditionally reset `dev_fee_paid = false` and cleared the payment hash.
However, a timeout does not mean the payment failed — the Lightning payment
could still be in-flight or may have succeeded after the timeout window.

This created a race condition:

1. Payment initiated, times out locally (but still in-flight on LN)
2. Code resets order to unpaid state
3. Original payment succeeds on LN (but local state already reset)
4. Next scheduler cycle finds the order as "unpaid" and initiates a second payment
5. **Result: double payment**

See [Issue #568](https://github.com/MostroP2P/mostro/issues/568) for full details.

## Solution

### Two-phase payment flow (`src/app/release.rs`, `src/scheduler.rs`)

The dev fee payment is split into two phases:

1. **Resolve phase** (`resolve_dev_fee_invoice`): LNURL resolution + invoice
   decode to extract the real LN payment hash.
2. **Send phase** (`send_dev_fee_payment`): Sends the pre-resolved invoice via
   LND.

The real payment hash is stored in `dev_fee_payment_hash` **before** the payment
is dispatched. This ensures that on timeout or crash, the hash is always
available for querying LND.

### `LndConnector::check_payment_status()` (`src/lightning/mod.rs`)

Queries the LN node for the current status of a payment using `TrackPaymentV2`.
Returns the LND `PaymentStatus` enum (Succeeded, InFlight, Failed, Unknown).

### `check_dev_fee_payment_status()` (`src/scheduler.rs`)

Helper that:
1. Extracts the payment hash from the order (skips `PENDING-` markers, which
   are legacy placeholders that cannot be tracked on LND)
2. Decodes the hex hash to bytes
3. Queries LND with a 10-second timeout
4. If payment succeeded: marks order as paid in DB
5. Returns a `DevFeePaymentState` enum for the caller

With the two-phase flow, new payments always have a real hash stored before
sending, so step 1 passes through to the LND query. The `PENDING-` guard
remains only for backward compatibility with legacy markers from before this
change — those correctly return `DevFeePaymentState::Unknown` since there is
genuinely no trackable hash.

### Timeout handler in `job_process_dev_fee_payment()` (`src/scheduler.rs`)

Instead of unconditionally resetting on timeout:

| LN Payment Status | Action |
|-------------------|--------|
| **Succeeded** | Mark as paid in DB, do NOT reset |
| **InFlight** | Skip reset, leave state intact (payment may still complete) |
| **Failed** | Safe to reset `dev_fee_paid = false` and retry |
| **Unknown** | Skip reset to err on the side of caution (avoid duplicate) |

### Stale real-hash cleanup (`src/scheduler.rs`)

A cleanup pass runs each cycle for orders that have `dev_fee_paid = true` and a
real (non-PENDING) payment hash. This handles crash recovery: if the process
crashes between storing the hash and receiving LND confirmation, the cleanup
queries LND and resets failed payments for retry.

### Design Principle

**When in doubt, don't retry.** A missed dev fee payment can be recovered
manually, but a duplicate payment is money lost. The code errs on the side of
caution — only resetting when the LN node confirms the payment definitively
failed.

## Related

- Issue: [#568](https://github.com/MostroP2P/mostro/issues/568)
- Dev fee invoice resolution: `src/app/release.rs` (`resolve_dev_fee_invoice`)
- Dev fee payment send: `src/app/release.rs` (`send_dev_fee_payment`)
- Dev fee scheduler: `src/scheduler.rs` (`job_process_dev_fee_payment`)
- Dev fee documentation: `docs/DEV_FEE.md`
