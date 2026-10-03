-- Reputation portability (docs/REPUTATION_PORTABILITY.md, PR 3.2).
--
-- Backfill for rows that predate the previous migration. There is no
-- per-review history to replay, so a legacy row's native rating sum is its
-- displayed average times its reviews: it then exports what counterparties
-- already see (plan section 7, "Legacy rows"). Its native date is the date
-- it shows.
UPDATE users SET native_rating_sum = total_rating * total_reviews
  WHERE native_rating_sum = 0.0 AND total_reviews > 0;
UPDATE users SET native_created_at = created_at WHERE native_created_at IS NULL;

-- One row per import. `issuer` is the trust-list entry's name, which the two
-- unique indexes deduplicate on, so an issuer rotating its key cannot reopen
-- an account to a second import; `issuer_key` is the key that signed, which
-- revoking a compromised key selects by, with `imported_at`, the node's own
-- clock (never the attestation's created_at, which the key holder chooses).
-- The figures are kept so an import can be reversed exactly.
CREATE TABLE IF NOT EXISTS reputation_imports (
  attestation_id char(64) primary key not null,
  issuer text not null,
  issuer_key char(64) not null,
  subject text not null,
  identity_pubkey char(64) not null,
  reviews integer not null,
  rating_hundredths integer not null,
  since integer not null,
  imported_at integer not null
);
CREATE UNIQUE INDEX IF NOT EXISTS reputation_imports_issuer_subject
  ON reputation_imports (issuer, subject);
CREATE UNIQUE INDEX IF NOT EXISTS reputation_imports_issuer_identity
  ON reputation_imports (issuer, identity_pubkey);
CREATE INDEX IF NOT EXISTS reputation_imports_issuer_key
  ON reputation_imports (issuer_key, imported_at);
