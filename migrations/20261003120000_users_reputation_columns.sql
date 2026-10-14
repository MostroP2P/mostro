-- Reputation portability (docs/REPUTATION_PORTABILITY.md, PR 3.2): what a
-- user imported is kept apart from what they earned here, so an import can be
-- reversed exactly and is never exported again as native reputation.
--   seeded_reviews / seeded_rating_sum: what imports added.
--   native_rating_sum: raw sum of the native ratings (backfilled next file).
--   native_created_at: when the row was created; an import never moves it,
--     and reversing one restores created_at from it (backfilled next file).
--   reputation_exported_to / reputation_exported_at: as an issuer, the
--     identity the account's reputation is bound to and the day of the last
--     export (phase 4).
-- Only ADD COLUMN here, so a re-run on a database that already has them is
-- reconciled by db::connect.
ALTER TABLE users ADD COLUMN seeded_reviews integer not null default 0;
ALTER TABLE users ADD COLUMN seeded_rating_sum real not null default 0.0;
ALTER TABLE users ADD COLUMN native_rating_sum real not null default 0.0;
ALTER TABLE users ADD COLUMN native_created_at integer;
ALTER TABLE users ADD COLUMN reputation_exported_to char(64);
ALTER TABLE users ADD COLUMN reputation_exported_at integer;
