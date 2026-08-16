-- Timestamp (unix seconds) when a buyer payout was claimed, sealed together
-- with `payout_payment_hash` in the same CAS.
--
-- Reconciliation ignores a marker younger than a grace window so a just-claimed
-- payout is never misread as "unknown to LND" during the brief window between
-- the claim and LND registering the payment. Without it, a reconciliation tick
-- landing in that window could clear the marker and let a second payout be
-- dispatched for the same escrow (a scriptable timing race).
ALTER TABLE orders ADD COLUMN payout_claimed_at integer;
