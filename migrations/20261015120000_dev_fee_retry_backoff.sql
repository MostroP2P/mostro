-- Per-order backoff for dev fee payments (issue #1063).
--
-- Before this, the dev fee job retried every unpaid dev fee on every cycle
-- (60s), resolving a fresh LNURL invoice and sending it each time. While the
-- Lightning node could not pay (2026-10-14: LND stuck on a stale chain), that
-- meant hundreds of failed payments an hour, in bursts.
--
-- `dev_fee_failed_attempts` counts consecutive failed attempts (resolution or
-- payment) for the order; `dev_fee_next_attempt_at` (unix seconds) is the
-- earliest time `find_unpaid_dev_fees` hands the order out again. NULL means
-- "no backoff": every existing row is immediately eligible, as before.
ALTER TABLE orders ADD COLUMN dev_fee_failed_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE orders ADD COLUMN dev_fee_next_attempt_at INTEGER;
