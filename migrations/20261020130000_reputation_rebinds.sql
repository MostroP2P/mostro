-- Rebind authorisations an owner's move applied (docs/REPUTATION_PORTABILITY.md
-- section 5.2, "Rebinding"). `id` is the authorisation's event id, kept until
-- `valid_until`, the last second core still accepts it (its expiration plus
-- the clock skew allowance), so each one moves a binding once, even if the
-- binding comes back to its signer meanwhile.
CREATE TABLE IF NOT EXISTS reputation_rebinds (
  id char(64) primary key not null,
  valid_until integer not null
);
