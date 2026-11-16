-- Track A TA-1f — Cashu fee collection (Option 2, funder-pays-at-lock).
--
-- The seller funds the WHOLE Mostro fee (`2 * order.fee`) as a separate
-- P2PK-1-of-1 token locked to Mostro's arbitrator key, submitted alongside the
-- 2-of-3 escrow in the same `AddCashuEscrow`. These columns persist that fee
-- token crash-safely, in the SAME atomic write as the lock, so a redeem
-- interrupted by a crash between lock and swap is retryable from the DB (a
-- scheduler job picks up `cashu_fee_token IS NOT NULL AND
-- cashu_fee_redeemed_at IS NULL`). All NULL for Lightning orders and for
-- fee-free (`mostro.fee == 0`) Cashu nodes.
--
--  * cashu_fee_token       Serialized P2PK-1-of-1 fee token (Mostro's revenue).
--  * cashu_fee_redeemed_at Unix seconds when Mostro redeemed the fee token; NULL
--                          until collected.
ALTER TABLE orders ADD COLUMN cashu_fee_token text;
ALTER TABLE orders ADD COLUMN cashu_fee_redeemed_at integer;

-- Cross-order anti-reuse guard. The fee token is P2PK to the node-wide `P_M`
-- (unlike the escrow token, whose 2-of-3 embeds per-order trade keys), so the
-- NUT-07 unspent check only stops SEQUENTIAL reuse. Two concurrent
-- `AddCashuEscrow` for two same-fee orders could both validate the same fee
-- token before the first redeem (TOCTOU). Recording each fee proof's
-- `Y = hash_to_curve(secret)` with a UNIQUE primary key, inserted in the SAME
-- transaction as the lock CAS, makes the second submission fail cleanly.
CREATE TABLE IF NOT EXISTS cashu_fee_proofs (
    y          text    PRIMARY KEY,
    order_id   text    NOT NULL,
    created_at integer NOT NULL
);
