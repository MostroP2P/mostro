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
  -- 'pending-payout' | 'slashed' | 'forfeited' | 'failed'
  state            varchar(16) not null,
  -- BondSlashReason: 'lost-dispute' | 'timeout'. Set on entry to
  -- 'pending-payout' and never cleared, so non-NULL while state is
  -- 'pending-payout', 'slashed', 'forfeited', or 'failed'.
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
  -- Phase 3: portion of `amount_sats` that the node retains on slash. Frozen
  -- at the moment the bond enters `pending-payout` so a later config change
  -- or daemon restart cannot re-balance the split. NULL for any bond that
  -- never reached `pending-payout`. The counterparty share is always derived
  -- as `amount_sats - node_share_sats` so they cannot drift.
  node_share_sats  integer,
  -- Phase 3: counts ONLY `send_payment` retries against an invoice the
  -- counterparty has already submitted. `payout_max_retries` is checked
  -- against this counter alone — invoice-request DMs do NOT count here
  -- (see `invoice_request_attempts` below).
  payout_attempts  integer not null default 0,
  -- Phase 3: counts how many `Action::AddInvoice` DMs the scheduler has
  -- sent asking the counterparty for a payout invoice. Bounded by the
  -- forfeit window (`payout_claim_window_days`), not by `payout_max_retries`,
  -- so a slow-responding counterparty cannot prematurely flip the bond to
  -- `failed`. Reset to 0 when the invoice is finally received.
  invoice_request_attempts integer not null default 0,
  -- Phase 3: timestamp of the last `AddInvoice` DM. Drives the
  -- `payout_invoice_window_seconds` cadence check ("don't re-DM before the
  -- window has elapsed"). Persisted so a daemon restart doesn't trigger an
  -- immediate re-DM.
  last_invoice_request_at integer,
  locked_at        integer,
  released_at      integer,
  -- Set on entry to `pending-payout` (i.e. when the slash decision is
  -- made), not on the later `slashed` transition. Anchors the
  -- `payout_claim_window_days` forfeit deadline (see Phase 3).
  slashed_at       integer,
  created_at       integer not null,
  -- Phase 1 concurrent-bonds taker context. Under the concurrent-bonds
  -- model (see `docs/ANTI_ABUSE_BOND.md`), multiple `Requested` taker
  -- bonds may coexist on a single order while the takers race to lock.
  -- Until one bond actually locks, the order's taker-flow fields are
  -- ambiguous (which racer "owns" them?), so the take handlers stash
  -- the deferred context here and `on_bond_invoice_accepted` promotes
  -- the winning bond's `taker_*` columns onto the order row.
  --
  -- Nullable so maker bonds (Phase 5+) and child slash rows (Phase 6)
  -- — neither of which carries take context — leave them at `NULL`.
  taker_identity     char(64),
  taker_trade_index  integer,
  taker_invoice      text,
  taker_fiat_amount  integer,
  taker_amount       integer,
  taker_fee          integer,
  taker_dev_fee      integer,
  FOREIGN KEY(order_id) REFERENCES orders(id)
);

CREATE INDEX IF NOT EXISTS idx_bonds_order_id ON bonds(order_id);
CREATE INDEX IF NOT EXISTS idx_bonds_state    ON bonds(state);
CREATE INDEX IF NOT EXISTS idx_bonds_parent   ON bonds(parent_bond_id);
