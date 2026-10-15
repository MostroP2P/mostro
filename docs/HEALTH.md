# Health Monitoring

How mostrod notices that it cannot do its job, what it does about it, and how it tells the outside world (issue #1063).

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
- when LND stopped answering (`unreachable_since`), and the last `GetInfo` it did answer;
- when the current run of "cannot pay", unsynced or unreachable in any mix, began (`unhealthy_since`), the one clock outgoing payments are deferred on, and when the last such run that lasted long enough to defer payments ended (`deferral_ended_at`), so a failure that arrives after LND recovered can still be recognised as caused by that run.

A broken gRPC connection is rebuilt on the next poll. Transitions (synced/unsynced, reachable/unreachable) are logged once, at `warn` when something went wrong.

An observation older than three polls is not trusted: if the monitor itself stopped, nothing is deferred and the `lnd_sync` check is `unknown`.

## Outgoing payments wait

Once LND has been unsynced or has not answered, in any mix, for **60 seconds** without a synced answer in between (`PAYMENT_DEFER_AFTER_SECS`), outgoing payments wait instead of failing. Moving between the two unhealthy states does not restart that clock. At startup there is no grace period: mostrod seeds this state from the `GetInfo` it makes while booting, and if LND is unsynced then, payments wait from the first moment, before any payment job runs. LND reports `synced_to_chain = false` for a few seconds whenever a block arrives; that does not hold anything back.

| Payment | While deferred |
|---|---|
| Buyer payout (`do_payment`, after `release` or `admin-settle`, or a new invoice from the buyer) | Not sent. The order stays in `settled-hold-invoice` with `failed_payment = true` and `payment_attempts` unchanged, so `job_retry_failed_payments` pays it once LND recovers. The buyer is not notified. |
| Buyer payout that fails anyway (it was already in flight when LND went bad) | Recorded the same way: no attempt consumed, no `payment-failed`, no `add-invoice`. A failure that arrives after LND recovered is treated the same if payments were deferred at any moment since the send (`payout_claimed_at` bounds the send). |
| Bond payout that fails anyway | Same rule, judged from `bonds.payout_sent_at`, stamped with the payment hash right before the send: no retry consumed, the invoice is kept, and a terminal failure clears the hash so the next tick sends the same invoice again. |

The start and end of the last deferring run are persisted in `daemon_state` (`lnd_deferral_started_at`, `lnd_deferral_ended_at`) and loaded at startup, so a payout sent before a run that ended while the daemon was down is still not counted against anyone. A run still open at shutdown continues if LND is still unhealthy at startup, and is taken as ended at startup if LND is synced by then.
| `job_retry_failed_payments` | Skips its pass. |
| Dev fee job, bond payout job | Skip their cycle. |

## Fail closed: automatic maintenance

When LND has been unsynced for `[health].lnd_unsynced_fail_after` seconds (600 by default), the `lnd_sync` check is `fail` and the daemon enters maintenance mode with the reserved reason `lnd_unsynced`: new orders and takes are rejected, open trades can still be released, canceled and disputed. It leaves maintenance once LND has been synced for `[health].lnd_resynced_clear_after` seconds (300 by default), **only if it is still the daemon's own** (a separate `maintenance_automatic` marker, never inferred from the reason). Maintenance an operator set is never overwritten or lifted, and `SetMaintenanceMode` refuses the reserved reason `lnd_unsynced`. See [MAINTENANCE_MODE_LN_MIGRATION.md](MAINTENANCE_MODE_LN_MIGRATION.md) §3.8.

An LND that does not answer at all makes the check `unknown`, not `fail`, so it does not turn maintenance on by itself; outgoing payments still wait.

Set `[health].auto_maintenance_on_lnd_unsynced = false` to opt out.

## The `lnd_sync` check

Computed from the sync state above; it is what turns automatic maintenance on (`fail`). It is the `lnd_sync` check of the health event (kind 38389, protocol `health_event.md`), which publishes it (see [The health event](#the-health-event-kind-38389)).

| State | When |
|---|---|
| `ok` | `synced_to_chain` is `true` |
| `warn` | `synced_to_chain` has been `false` for less than `lnd_unsynced_fail_after` seconds without interruption |
| `fail` | `synced_to_chain` has been `false` for `lnd_unsynced_fail_after` seconds or more without interruption |
| `unknown` | `GetInfo` failed or timed out, or the last observation is stale |

## The health event (kind 38389)

The spec is `health_event.md` in the protocol book (MostroP2P/protocol#83). mostrod publishes it from `job_health_event_send` (`src/scheduler.rs`), implemented in `src/app/health/publisher.rs`, in both Lightning and Cashu mode, unless `[health].enabled = false`.

**Cadence.** The checks are evaluated every 15 seconds; every database read is skipped once the `SELECT 1` probe fails, and the probe and all the reads share one 5 s budget, so a stuck database is reported in time and whenever maintenance mode changes. The event is published every `[health].interval` seconds (60 by default), and at once when `status`, a check state or `maintenance_mode` changes, but never twice within 10 seconds. It goes to the same relays as the info event. A send no relay accepts does not count as a publication and is retried at the next evaluation.

**Public tags** (`src/app/health/event.rs`): `d` = the Mostro pubkey, `y`, `z` = `health`, `status`, one `check` per check with the time it entered its state, `escrow_mode`, `lnd_block_height` / `lnd_synced_to_chain` / `lnd_network` (Lightning, when LND answered), `maintenance_mode`, `mostro_version`, `interval`, `seq`, `p` (only with a watchdog key) and `expiration` = `created_at` + 3 × `interval`. No tag is ever repeated.

**Checks** (`src/app/health/check.rs`, `src/app/health/cashu_mint.rs`):

| Check | Mode | Rule |
|---|---|---|
| `lnd_sync` | Lightning | see [above](#the-lnd_sync-check) |
| `payouts` | Lightning | `fail` when a settled-but-unpaid order is over 2 h old and no payout succeeded in 2 h, or when more than half of at least 3 payout attempts in the last hour failed; `warn` when a settled-but-unpaid order is over 15 min old; otherwise `ok`; `unknown` if the database cannot be read |
| `cashu_mint` | Cashu | probes `GET /v1/info` (10 s) on the `[cashu].mint_urls` allowlist plus the distinct mints currently holding escrow (an escrow token locked on an order that is not terminal; never a mint a pending order merely names), so a de-listed mint still holding escrow keeps being probed; each is reached the way the daemon connects to it (on an open node, with the public-host check); `ok` all answer (or none to probe), `warn` some, `fail` none, `unknown` when the orders cannot be read |
| `relays` | both | `ok` all connected, `warn` some, `fail` none |
| `database` | both | `SELECT 1` within 5 s |

`status` is `down` when `lnd_sync` is `fail` or `unknown`, `cashu_mint`, `database` or `relays` is `fail`; otherwise `degraded` when any check is not `ok` or maintenance is on; otherwise `ok`.

**Encrypted content** (`src/app/health/content.rs`). With `[health].watchdog_pubkey` set, `content` is NIP-44 v2 to that key: LND state (peers, channels, liquidity over active channels, pending HTLCs and the nearest expiry in blocks), buyer payouts over the last hour with failure reasons, the last successful payout, settled-but-unpaid orders and the oldest one's age, orders waiting for the buyer's new invoice, pending and failed dev fees, relays (with the last outage) and maintenance (with its reason). Unknown values are `null`, never `0`. Without a key, `content` is empty and there is no `p` tag.

**Where the numbers come from.**

- Payout attempts are counted where buyer payouts reach a final state in LND: the payout watcher and the in-flight reconciliation in `src/app/release.rs` (`HealthStats` in `src/app/health/stats.rs`, an in-memory one-hour window). A deferred payout is not an attempt.
- The last successful payout time, `seq` and the last `created_at`, and the last relay outage are persisted in `daemon_state`, so they survive a restart.
- The age of a settled order is measured from `orders.hold_settled_at`, stamped when the hold invoice is settled (`release`, `admin-settle`); orders settled before that column existed fall back to `invoice_held_at`.
- `lnd` data comes from the sync monitor's last `GetInfo` and a `ListChannels` call.

**Clock.** `created_at` is the clock, but never lower than the previous `created_at` plus one, so relays never keep an older revision after the clock steps back. If that puts it more than 300 s ahead of the clock, mostrod still publishes and logs an error: readers ignore the event until the clock catches up.

**What this does not do.** mostrod reports what its own node tells it. A node on a stale chain does not necessarily know it, so a watcher should compare `lnd_block_height` with an independent reference (MostroP2P/mostro-watchdog#46).

## Configuration

```toml
[health]
enabled = true
interval = 60
watchdog_pubkey = ""
lnd_unsynced_fail_after = 600
auto_maintenance_on_lnd_unsynced = true
lnd_resynced_clear_after = 300
```

See [STARTUP_AND_CONFIG.md](STARTUP_AND_CONFIG.md) for each key.

## Dev fee retry backoff

Independently of LND's state, a dev fee whose attempt failed waits before the next one (60 s doubling up to 1 h, plus jitter). See [DEV_FEE.md](DEV_FEE.md#retry-backoff).
