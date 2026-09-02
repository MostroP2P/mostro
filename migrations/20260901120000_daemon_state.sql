-- Small key/value store for operator-controlled daemon state that must
-- survive a restart but is flipped at runtime (not from settings.toml).
--
-- First user: maintenance mode (docs/MAINTENANCE_MODE_LN_MIGRATION.md).
-- `maintenance_mode` = "0" | "1", `maintenance_reason`, `maintenance_since`
-- (unix seconds). Later phases add `ln_node_pubkey` for the boot guard.
CREATE TABLE IF NOT EXISTS daemon_state (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
