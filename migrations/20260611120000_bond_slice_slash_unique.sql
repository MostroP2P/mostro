-- Phase 6 hardening: enforce the "one slash row per slice" invariant at the
-- schema level, not just in `record_maker_slice_slash`'s atomic
-- `INSERT ... WHERE NOT EXISTS`.
--
-- The application insert already wins/loses the TOCTOU race correctly (the
-- loser sees `rows_affected = 0`), but a unique index makes the invariant
-- hold for ANY future caller (e.g. the Phase 7 maker-timeout slash) and
-- survives a code regression that drops the existence check.
--
-- Partial so it constrains ONLY child slash rows: SQLite treats NULLs as
-- distinct, so a plain unique index on (parent_bond_id, child_order_id) would
-- already exempt parent rows (both NULL), taker bonds (both NULL), and
-- maker-refund rows (child_order_id NULL). The explicit `WHERE … IS NOT NULL`
-- predicate keeps the index small (only the child rows it governs) and makes
-- the intent unmistakable.
CREATE UNIQUE INDEX IF NOT EXISTS idx_bonds_parent_child_unique
  ON bonds (parent_bond_id, child_order_id)
  WHERE parent_bond_id IS NOT NULL AND child_order_id IS NOT NULL;
