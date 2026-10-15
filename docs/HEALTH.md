# Health Monitoring

How mostrod notices that it cannot do its job, and what it does about it (issue #1063).

## Why

On 2026-10-14 a production node ran LND with the neutrino backend. Neutrino picked a sync peer that advertised a bogus best height, the peer disconnected and block sync never resumed: LND stayed at height 971349 with `synced_to_chain = false` for about 21 hours while the chain moved on by 136 blocks.

With a stale block height every outgoing HTLC carried a CLTV expiry the receiver rejected, so **every buyer payout and every dev fee failed** (`IncorrectOrUnknownPaymentDetails`). Hold invoices were still created and incoming payments still arrived, and the info event kept being published, so from the outside everything looked normal. Meanwhile:

- sellers' sats were locked in new trades the daemon could not pay out;
- each failed payout consumed one of the buyer's `payment_attempts`, and after three the buyer was asked for a new invoice for a failure that was ours;
- the dev fee job re-sent every unpaid dev fee every minute, hundreds of failed payments an hour.

## LND sync monitor

`job_lnd_sync_monitor` (`src/scheduler.rs`, Lightning mode only) calls LND's `GetInfo` every 15 seconds, bounded to 10 seconds, and records the answer in `LndSyncState` (`src/app/health/lnd_sync.rs`), shared through `AppContext::lnd_sync()`:

- when the current uninterrupted run of `synced_to_chain = false` began (`unsynced_since`);
- when the current run of `synced_to_chain = true` began (`synced_since`);
- when LND stopped answering (`unreachable_since`), and the last `GetInfo` it did answer.

A broken gRPC connection is rebuilt on the next poll. Transitions (synced/unsynced, reachable/unreachable) are logged once, at `warn` when something went wrong.

An observation older than three polls is not trusted: if the monitor itself stopped, nothing is deferred and the `lnd_sync` check is `unknown`.

## Outgoing payments wait

Once LND has been unsynced or has not answered, in any mix, for **60 seconds** without a synced answer in between (`PAYMENT_DEFER_AFTER_SECS`), outgoing payments wait instead of failing. Moving between the two unhealthy states does not restart that clock. LND reports `synced_to_chain = false` for a few seconds whenever a block arrives; that does not hold anything back.

| Payment | While deferred |
|---|---|
| Buyer payout (`do_payment`, after `release` or `admin-settle`, or a new invoice from the buyer) | Not sent. The order stays in `settled-hold-invoice` with `failed_payment = true` and `payment_attempts` unchanged, so `job_retry_failed_payments` pays it once LND recovers. The buyer is not notified. |
| Buyer payout that fails anyway (it was already in flight when LND went bad) | Recorded the same way: no attempt consumed, no `payment-failed`, no `add-invoice`. |
| `job_retry_failed_payments` | Skips its pass. |
| Dev fee job, bond payout job | Skip their cycle. |

## Fail closed: automatic maintenance

When LND has been unsynced for `[health].lnd_unsynced_fail_after` seconds (600 by default), the `lnd_sync` check is `fail` and the daemon enters maintenance mode with the reserved reason `lnd_unsynced`: new orders and takes are rejected, open trades can still be released, canceled and disputed. It leaves maintenance once LND has been synced for `[health].lnd_resynced_clear_after` seconds (300 by default), **only if the reason is still `lnd_unsynced`**. Maintenance an operator set is never overwritten or lifted. See [MAINTENANCE_MODE_LN_MIGRATION.md](MAINTENANCE_MODE_LN_MIGRATION.md) §3.8.

An LND that does not answer at all makes the check `unknown`, not `fail`, so it does not turn maintenance on by itself; outgoing payments still wait.

Set `[health].auto_maintenance_on_lnd_unsynced = false` to opt out.

## The `lnd_sync` check

| State | When |
|---|---|
| `ok` | `synced_to_chain` is `true` |
| `warn` | `synced_to_chain` has been `false` for less than `lnd_unsynced_fail_after` seconds without interruption |
| `fail` | `synced_to_chain` has been `false` for `lnd_unsynced_fail_after` seconds or more without interruption |
| `unknown` | `GetInfo` failed or timed out, or the last observation is stale |

## Configuration

```toml
[health]
lnd_unsynced_fail_after = 600
auto_maintenance_on_lnd_unsynced = true
lnd_resynced_clear_after = 300
```

See [STARTUP_AND_CONFIG.md](STARTUP_AND_CONFIG.md) for each key.

## Dev fee retry backoff

Independently of LND's state, a dev fee whose attempt failed waits before the next one (60 s doubling up to 1 h, plus jitter). See [DEV_FEE.md](DEV_FEE.md#retry-backoff).
