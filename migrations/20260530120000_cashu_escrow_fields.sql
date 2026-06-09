-- mostro-core 0.12.1 adds three Cashu escrow fields to the `Order` model.
-- These columns back the Cashu 2-of-3 multisig escrow mode (see the escrow
-- architecture spec). They are `NULL` for Lightning orders.
--
-- * cashu_mint_url         URL of the Cashu mint hosting the escrow.
-- * cashu_escrow_token     Serialized Cashu 2-of-3 multisig token held as escrow.
-- * cashu_escrow_locked_at Unix timestamp (seconds) when the escrow token was
--                          validated and locked in.
ALTER TABLE orders ADD COLUMN cashu_mint_url text;
ALTER TABLE orders ADD COLUMN cashu_escrow_token text;
ALTER TABLE orders ADD COLUMN cashu_escrow_locked_at integer;
