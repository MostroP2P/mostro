-- Phase 0 additive migration for concurrent taker bonds.
--
-- Under the concurrent-bonds model (see `docs/ANTI_ABUSE_BOND.md`),
-- multiple `Requested` taker bonds may coexist on a single order while
-- the takers race to lock. Until one bond actually locks, the order's
-- `taker_*` fields are ambiguous (which racer "owns" them?), so the
-- take handlers stop persisting taker context onto the `orders` row
-- and stash it here on the bond row instead. The winning bond's
-- columns are copied onto the order at lock-time by
-- `on_bond_invoice_accepted` / `resume_take_after_bond`.
--
-- All columns are nullable: bonds created before this migration ran
-- (Phase 1 supersede-era rows) have no take context to recover, and
-- maker bonds (Phase 5+) never carry take context.

ALTER TABLE bonds ADD COLUMN taker_identity char(64);
ALTER TABLE bonds ADD COLUMN taker_trade_index integer;
ALTER TABLE bonds ADD COLUMN taker_invoice text;
ALTER TABLE bonds ADD COLUMN taker_fiat_amount integer;
ALTER TABLE bonds ADD COLUMN taker_amount integer;
ALTER TABLE bonds ADD COLUMN taker_fee integer;
ALTER TABLE bonds ADD COLUMN taker_dev_fee integer;
