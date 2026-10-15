-- Indexes for the queries the health event runs every 15 seconds (issue
-- #1063), so they do not scan every order ever made.
--
-- * `idx_orders_status`: settled-but-unpaid orders are a handful among all
--   orders; the payout aggregate filters on `status = 'settled-hold-invoice'`.
-- * `idx_orders_unpaid_dev_fee`: a partial index over the orders whose dev
--   fee is still unpaid (usually none), keyed by status. Used by the health
--   event's pending dev fee count and by the dev fee job's selection, which
--   otherwise walk every `success` order.
CREATE INDEX IF NOT EXISTS idx_orders_status ON orders(status);
CREATE INDEX IF NOT EXISTS idx_orders_unpaid_dev_fee ON orders(status)
  WHERE dev_fee > 0 AND dev_fee_paid = 0;
