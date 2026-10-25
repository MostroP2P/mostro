-- Payment-account history / anti-triangulation
-- (docs/PAYER_HISTORY_ANTI_TRIANGULATION.md §9).
--
-- A buyer commits to the fiat account it pays from by sending only a hash of
-- the canonicalised account details; the plaintext travels buyer -> seller
-- off-band and never reaches the daemon (D-2). These tables hold the hash
-- while the trade is open and, once a trade succeeds, aggregate counters per
-- (buyer identity key, payment hash) that the seller of a later trade reads.
--
-- Opt-in: the migration always applies (D-10, item 1), but nothing inserts or
-- updates a row here unless `[payer_history].enabled = true`. No foreign
-- keys: `orders` rows outlive declarations, and the bond tables set the
-- precedent of not declaring FKs.

-- Per-order commitment. Short-lived: consumed on success, pruned on any other
-- terminal status. Holds the ONLY copy of a hash that is not yet history.
CREATE TABLE IF NOT EXISTS order_payer_declarations (
  order_id        char(36)  PRIMARY KEY NOT NULL,  -- orders.id (uuid)
  payment_hash    char(64)  NOT NULL,              -- sha256 hex, lowercase
  declared_at     integer   NOT NULL,              -- unix secs of last upsert
  buyer_pubkey    char(64),                        -- orders.buyer_pubkey when declared; a later buyer never inherits the row
  history_snapshot text                            -- PaymentHistory JSON frozen at fiat-sent (§10.3); every later query returns it
);

-- Aggregate history. One row per (buyer identity key, payment hash), keyed by
-- payer_key = HMAC-SHA256(node secret, domain || payment_hash), never by the
-- hash itself: the hash is cheap to recompute from a candidate account, so
-- stored raw next to user_pubkey a leaked database would reveal which account
-- a buyer pays from (D-2). Written ONLY from the Success CAS in
-- release::payment_success.
CREATE TABLE IF NOT EXISTS payer_history (
  user_pubkey        char(64) NOT NULL,  -- orders.master_buyer_pubkey (reputation mode only)
  payer_key          char(64) NOT NULL,  -- payer_key(node secret, payment_hash), lowercase hex
  first_success_at   integer  NOT NULL,
  last_success_at    integer  NOT NULL,
  successful_trades  integer  NOT NULL DEFAULT 0,
  PRIMARY KEY (user_pubkey, payer_key)
);

-- Distinct-counterparty set. counterparty_id is a keyed hash (D-7), never a
-- pubkey. `experienced` is the D-7 qualification snapshot taken at success
-- time; while the threshold policy is unchanged it may flip 0 -> 1 on a LATER
-- success with the same triple, never retroactively and never back. A change
-- to (N, D) rewrites the whole column under the new policy (§10.7).
CREATE TABLE IF NOT EXISTS payer_history_counterparties (
  user_pubkey        char(64) NOT NULL,
  payer_key          char(64) NOT NULL,
  counterparty_id    char(64) NOT NULL,
  first_success_at   integer  NOT NULL,
  -- Newest success with this triple: the instant §10.7 re-evaluates at.
  last_success_at    integer  NOT NULL,
  experienced        integer  NOT NULL DEFAULT 0,  -- 1 = counterparty qualified (D-7)
  -- Generation of (N, D) this row's `experienced` was evaluated under. Only
  -- rows of the current generation count toward experienced_counterparties.
  policy_gen         integer  NOT NULL,
  PRIMARY KEY (user_pubkey, payer_key, counterparty_id)
);

-- Threshold policy the `experienced` column was last evaluated under (D-7).
-- Single row (id = 1). Its only purpose is to detect a configuration change
-- across restarts so §10.7 can recompute instead of leaving a mixed
-- population of old-policy and new-policy snapshots.
CREATE TABLE IF NOT EXISTS payer_history_policy (
  id                     integer PRIMARY KEY CHECK (id = 1),
  generation             integer NOT NULL,  -- bumped on every (N, D) change; stamped into policy_gen
  experienced_min_trades integer NOT NULL,
  experienced_min_days   integer NOT NULL,
  evaluated_at           integer NOT NULL,  -- unix secs of the last (re)evaluation
  node_key_id            text               -- node_key_id() of the secret payer_key / counterparty_id were checked under; NULL until the first recompute; a change discards the history
);

-- D-7 qualification reads every undisputed success of one seller
-- (`seller_experience`), on each payer-history success and once per stored
-- snapshot when the thresholds change. Without an index that is a full scan
-- of `orders` each time.
CREATE INDEX IF NOT EXISTS idx_orders_master_seller_pubkey ON orders(master_seller_pubkey);
