-- Anti-abuse bond storage (issue #711, Phase 0).
--
-- The bond is an opt-in hold invoice locked by a trading party (taker, maker,
-- or both — controlled by the `[anti_abuse_bond]` settings block). Rows in
-- this table are only created when the feature is enabled; existing orders
-- are unaffected.
--
-- Later phases will hook trade flows into this table:
--   Phase 1: taker bond lock + always-release.
--   Phase 2: taker bond slash on lost dispute.
--   Phase 3: payout flow to the winning counterparty.
--   Phase 4: taker timeout slash.
--   Phase 5: maker bond.
--   Phase 6: range-order proportional slashes (parent/child rows).
--   Phase 7: maker timeout slash.
CREATE TABLE IF NOT EXISTS bonds (
  id               char(36) primary key not null,
  order_id         char(36) not null,
  -- Parent bond id for Phase 6 range-order child slashes. NULL for parent /
  -- non-range bonds. Self-referential FK is intentionally not declared so
  -- that child rows can be created independently of their parent lifecycle.
  parent_bond_id   char(36),
  -- Child order id used by Phase 6 when a single maker parent bond records
  -- proportional slashes against individual children. NULL for parent /
  -- non-range bonds.
  child_order_id   char(36),
  -- Trade pubkey of the bonded party. Not the identity pubkey.
  pubkey           char(64) not null,
  -- 'maker' | 'taker'
  role             varchar(8) not null,
  -- Amount (sats) covered by this bond row. For a parent bond this is the
  -- total locked; for a child slash row this is the proportional slice.
  amount_sats      integer not null,
  -- Running total of sats already transitioned to a slashed child. Used on
  -- parent bonds in Phase 6 to decide between full-settle and
  -- settle+refund at parent close. Unused (0) for child / non-range rows.
  slashed_share_sats integer not null default 0,
  -- BondState serialization: 'requested' | 'locked' | 'released' |
  -- 'pending-payout' | 'slashed' | 'failed'
  state            varchar(16) not null,
  -- BondSlashReason: 'lost-dispute' | 'timeout'. NULL unless state in
  -- ('pending-payout', 'slashed').
  slashed_reason   varchar(16),
  -- Bond hold invoice hash (hex). NULL until the hold invoice is created.
  hash             char(64),
  -- Preimage retained by Mostro so a slash does not depend on cooperation
  -- from the bonded party. NULL for child rows that share a parent HTLC.
  preimage         char(64),
  -- bolt11 payment request shown to the bonded party.
  payment_request  text,
  -- bolt11 payout invoice from the winning counterparty (Phase 3+).
  payout_invoice   text,
  -- Routing-fee ceiling actually used for the payout attempt (sats). NULL
  -- until the scheduler tries to pay the winner.
  payout_routing_fee_sats integer,
  -- Number of payout invoice requests attempted so far.
  payout_attempts  integer not null default 0,
  locked_at        integer,
  released_at      integer,
  slashed_at       integer,
  created_at       integer not null,
  FOREIGN KEY(order_id) REFERENCES orders(id)
);

CREATE INDEX IF NOT EXISTS idx_bonds_order_id ON bonds(order_id);
CREATE INDEX IF NOT EXISTS idx_bonds_state    ON bonds(state);
CREATE INDEX IF NOT EXISTS idx_bonds_parent   ON bonds(parent_bond_id);
