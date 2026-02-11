# Dev Fee Timeout Safety — Duplicate Payment Prevention

## Overview

When a dev fee Lightning payment times out, Mostrod now queries the LN node for
the actual payment status before deciding whether to reset and retry. This
prevents duplicate payments caused by race conditions between timeouts and
in-flight payments.

## Problem

Previously, when the 50-second timeout expired during a dev fee payment, the
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

### New: `LndConnector::check_payment_status()` (`src/lightning/mod.rs`)

Queries the LN node for the current status of a payment using `TrackPaymentV2`.
Returns the LND `PaymentStatus` enum (Succeeded, InFlight, Failed, Unknown).

### New: `check_dev_fee_payment_status()` (`src/scheduler.rs`)

Helper that:
1. Extracts the payment hash from the order (skips PENDING markers)
2. Decodes the hex hash
3. Queries LND with a 10-second timeout
4. If payment succeeded: marks order as paid in DB
5. Returns a `DevFeePaymentState` enum for the caller

### Modified: Timeout handler in `job_process_dev_fee_payment()` (`src/scheduler.rs`)

Instead of unconditionally resetting on timeout:

| LN Payment Status | Action |
|-------------------|--------|
| **Succeeded** | Mark as paid in DB, do NOT reset |
| **InFlight** | Skip reset, leave state intact (payment may still complete) |
| **Failed** | Safe to reset `dev_fee_paid = false` and retry |
| **Unknown** | Skip reset to err on the side of caution (avoid duplicate) |

### Design Principle

**When in doubt, don't retry.** A missed dev fee payment can be recovered
manually, but a duplicate payment is money lost. The code errs on the side of
caution — only resetting when the LN node confirms the payment definitively
failed.

## Related

- Issue: [#568](https://github.com/MostroP2P/mostro/issues/568)
- Dev fee payment flow: `src/app/release.rs` (`send_dev_fee_payment`)
- Dev fee scheduler: `src/scheduler.rs` (`job_process_dev_fee_payment`)
- Dev fee documentation: `docs/DEV_FEE.md`
