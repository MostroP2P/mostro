-- Track A TA-1: index the escrow token, and enforce "one order per escrow
-- token" at the schema level.
--
-- Two lookups added by the lock handler scan this column: the `NOT EXISTS`
-- leg of `update_order_cashu_escrow`'s compare-and-set, and
-- `cashu_escrow_token_in_use`. Both run on every `AddCashuEscrow`, and without
-- an index both are full table scans of `orders`.
--
-- Unique, not merely indexed, for the same reason as
-- `idx_bonds_parent_child_unique`: the application check already wins/loses
-- the TOCTOU race correctly (the loser sees `rows_affected = 0`), but the
-- index makes the invariant hold for ANY future caller and survives a code
-- regression that drops the `NOT EXISTS` predicate. It cannot change current
-- behaviour — the CAS never attempts a duplicate write — and it cannot fail on
-- existing data: `cashu_escrow_token` has had no writer in any released
-- version, since the only one is the handler this migration ships with.
--
-- Partial so it constrains ONLY locked escrows and stays small: every
-- Lightning-mode order leaves the column NULL. (SQLite treats NULLs as
-- distinct, so the predicate is about size and intent, not correctness.)
CREATE UNIQUE INDEX IF NOT EXISTS idx_orders_cashu_escrow_token
  ON orders (cashu_escrow_token)
  WHERE cashu_escrow_token IS NOT NULL;
