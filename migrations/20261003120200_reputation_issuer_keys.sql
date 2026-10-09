-- Reputation portability (docs/REPUTATION_PORTABILITY.md, PR 3.2): the
-- trust-list name each issuer key was first configured under, recorded at
-- boot whether or not the key has signed an import. Imports deduplicate on
-- that name, and the boot check compares every configured key with this
-- table as well as with reputation_imports, so a key that joined an entry in
-- a planned rotation cannot carry a renamed entry past the check once the
-- old key has left.
CREATE TABLE IF NOT EXISTS reputation_issuer_keys (
  issuer_key char(64) primary key not null,
  issuer text not null,
  first_seen_at integer not null
);
-- Keys that already signed imports are bound to the name they were recorded
-- under.
INSERT OR IGNORE INTO reputation_issuer_keys (issuer_key, issuer, first_seen_at)
  SELECT issuer_key, issuer, MIN(imported_at) FROM reputation_imports
  GROUP BY issuer_key, issuer;
