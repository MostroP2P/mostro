-- Proof-level uniqueness for Cashu escrows.
--
-- `orders.cashu_escrow_token` is unique as a string, but one set of ecash
-- proofs can be written as many different token strings (another memo,
-- another proof order, V3 or V4). Each proof is identified by
-- Y = hash_to_curve(secret), the same point the mint uses for NUT-07
-- checkstate, so this table makes a proof back at most one order's escrow.
-- Written in the same transaction as the escrow lock in
-- `update_order_cashu_escrow`.
CREATE TABLE IF NOT EXISTS cashu_escrow_proofs (
  y        char(66) PRIMARY KEY NOT NULL,  -- compressed secp256k1 point, lowercase hex
  order_id char(36) NOT NULL               -- orders.id (uuid) that holds the proof
);
CREATE INDEX IF NOT EXISTS idx_cashu_escrow_proofs_order ON cashu_escrow_proofs (order_id);
