# Reputation Portability

Design proposal for letting a user carry reputation earned in one trading venue
(lnp2pBot, or another Mostro instance) into a Mostro instance, without giving
any operator, or anyone who reads relays, a way to link the two identities.

Status: **proposal**. Values marked *open decision* are defaults to be
confirmed before implementation.

## 1. Goals

1. A user with reputation on lnp2pBot can claim it on a Mostro instance.
2. The same mechanism lets a user copy reputation from one Mostro to another.
3. No party can link the source identity (Telegram user, or identity pubkey on
   the source Mostro) with the destination identity pubkey. This must hold
   even when the source and destination operators are the same person, and
   even if the source database leaks.
4. Imported reputation is merged into the destination's ordinary reputation
   fields, so a counterparty sees one rating, one review count and one age, and
   never needs to know where it came from.
5. The destination operator decides which issuers to trust; nothing else is
   configurable per instance.

## 2. Non-goals

- Moving reputation. Portability is a **copy**: the source keeps it.
- Exact numbers. Reputation travels as coarse bands (section 4).
- Users in `full_privacy` mode. They have no identity-bound reputation by
  design, so there is nothing to import into.

## 3. Threat model

| Adversary | Must not learn |
|---|---|
| Source operator (bot or Mostro) | The destination identity pubkey, or which token was redeemed where |
| Destination operator | The source identity (`tg_id` or source pubkey), or exact source stats |
| Source + destination colluding, or a leaked source database | Any join key between the two identities |
| Relay observer / counterparty | Anything beyond the merged public rating |

Two attacks decide the design:

- **Fingerprinting by exact values.** "347 trades, 4.87 rating, 12.3 BTC" is
  unique in the source database. Any design that publishes exact values on the
  destination lets the source operator, or a past counterparty who saw those
  numbers in the bot, identify the destination pubkey. Hence bands.
- **Issuer sees what it signs.** If the issuer signs a plaintext containing the
  destination pubkey, or even a hash of it, it can recognise the same value
  when it is redeemed. Hence blind signatures.

Residual risk: **timing correlation**. With few migrations per day, a
colluding pair can match "issued at 14:02" with "redeemed at 14:05". The
client mitigates this by delaying redemption by a random interval, and issuers
store no issuance timestamps finer than the per-user flag in section 6.

## 4. Reputation bands

Reputation is quantised into a cell of a three-dimensional grid. The grid is a
**protocol constant**: every issuer uses the same cuts, so a token means the
same thing wherever it is redeemed. Operators choose whom to trust, not what a
band means.

| Dimension | Bands (*open decision*) | Seeds on the destination |
|---|---|---|
| Completed trades | `1-9`, `10-49`, `50-199`, `200+` | `total_reviews` = band floor |
| Average rating | `<4.0`, `4.0-4.5`, `4.5+` | `total_rating` = band floor |
| Account age | `<6m`, `6-24m`, `24m+` | `created_at` moved back to band floor |

Rules:

- Seeds always use the **floor** of the band. Nobody receives more than they
  earned; the destination shows a conservative lower bound.
- Cells are the leaves of the issuer keyset (section 5). An issuer must merge
  any cell holding fewer than `K` users (*open decision*, default 50) into an
  adjacent lower cell before publishing, so every cell is a real crowd.
- The trades dimension on Mostro counts orders that reached `success` for the
  identity; on lnp2pBot it is `trades_completed`.
- Eligibility (*open decision*): at least 10 completed trades, not banned, and
  disputes lost below 10% of completed trades. Below eligibility the issuer
  refuses and there is nothing to migrate.

## 5. Cryptography: blind signatures per cell

The scheme is BDHKE, the blind Diffie-Hellman key exchange used by Cashu. The
issuer plays the role of the mint, and each grid cell plays the role of a
denomination. Mostro already depends on `cdk`, which implements it; the bot can
use `@cashu/crypto`; the mobile app needs a few lines over secp256k1.

### 5.1 Issuer keyset

An issuer holds one secret scalar `k_cell` per grid cell and publishes the
public points `K_cell = k_cell·G` as a parameterised replaceable Nostr event
signed with the issuer's own key:

```
kind: 30xxx (open decision)
tags: ["d", "reputation-keyset"], ["epoch", "2026"]
content: {"cells": {"trades:200+|rating:4.5+|age:24m+": "<hex K>", ...}}
```

lnp2pBot publishes with its existing `NOSTR_SK`. A Mostro publishes with its
daemon key. Keysets are rotated by **epoch** (yearly); a destination accepts
the current and previous epoch only, so stale reputation cannot be imported
years later and an issuer can retire keys.

### 5.2 Issuance (export)

Runs on the user's device; the issuer never sees the plaintext.

1. Client builds `secret = "repv1:" || destination_identity_pubkey || nonce`.
2. Client computes `Y = hash_to_curve(secret)`, picks random `r`, sends
   `B_ = Y + r·G` to the issuer.
3. Issuer looks up the user in its own database, picks the cell, checks
   eligibility and the once-only flag, and returns `C_ = k_cell·B_` together
   with the cell id and a DLEQ proof that `C_` was produced with the published
   `K_cell`.
4. Client verifies the DLEQ against the public keyset. This defeats a
   **tagging attack** where an issuer signs one user with a private key to
   recognise them later.
5. Client unblinds `C = C_ - r·K_cell` and stores the token
   `{issuer, epoch, cell, secret, C}`.

Transport differs per issuer:

- **lnp2pBot**: the app opens `t.me/lnp2pbot?start=migrate_<base64(B_)>`;
  the bot answers with a deep link back into the app carrying `C_`, the cell
  id and the DLEQ. The bot sets `reputation_exported_at` on the user and
  stores nothing else.
- **Mostro**: a new gift-wrapped action `export-reputation` with `B_` in the
  payload, signed with the source identity key. The daemon answers with the
  same fields and sets `reputation_exported_at` in `users`.

### 5.3 Redemption (import)

A new gift-wrapped action `import-reputation`, signed with the **destination
identity key**, carrying the token. The destination daemon:

1. Checks the issuer is in its trusted list and the epoch is accepted.
2. Verifies `C == k_cell·hash_to_curve(secret)` using `K_cell` (i.e. verifies
   the unblinded signature against the published keyset).
3. Checks the pubkey embedded in `secret` equals the identity that signed the
   message. This binds the token to one identity; selling it means handing
   over the key, which is the same risk as selling the account today.
4. Checks `hash(secret)` is not in `redeemed_reputation_tokens`, and that this
   `(issuer, identity)` pair has not been redeemed before.
5. Inserts the redemption and seeds the user row (section 6) in the same
   transaction.

Replies: `reputation-imported` on success; existing error payload with a new
reason on failure (untrusted issuer, bad signature, already redeemed,
identity mismatch, expired epoch).

## 6. Merging on the destination

Each dimension has its own merge rule, because they measure different things:

- **Completed trades add up.** A trade on A and a trade on B are distinct
  trades, so the seed is added to the local count.
- **Rating is a weighted average**, weighted by review count per origin. It is
  never summed and never maxed.
- **Account age is a maximum, never a sum.** "Days operating" answers "since
  when does this person trade?", and that is a single date, the oldest one that
  can be proven. Time passes in parallel on every venue, so adding day counts
  would count the same month twice. A user who opened orders on A and B on the
  same 10 August has 30 days on both a month later; importing A into B leaves
  B at `max(30, 30) = 30`, not 60. With the default bands the import does not
  even move the date: 30 days falls in `<6m`, whose floor is 0. Only a user who
  proves 6+ months elsewhere moves `created_at`, and only when the local date
  is more recent than that floor.

Seeds are applied to the user's existing values; nothing is overwritten.

| `users` column | Effect |
|---|---|
| `total_reviews` | `+= trades_floor` |
| `total_rating` | recomputed as the weighted average of the existing average and the seed floor |
| `created_at` | `= min(created_at, now - age_floor)` |
| `min_rating`, `max_rating`, `last_rating` | unchanged if non-zero, otherwise set to `round(rating_floor)` |
| `seeded_reviews` (new) | `+= trades_floor`, internal only |
| `seeded_rating_sum` (new) | `+= trades_floor * rating_floor`, internal only |

The public `rating` tag on order events keeps its three-field shape:
`{"total_reviews", "total_rating", "since"}` (see 6.1). No `legacy` marker is published.
The seeded review count acts as an anchor, so new ratings move the average
slowly, exactly as they would for a long-standing Mostro user.

Worked example. User with 10 days on Mostro and 2 reviews at 4.5 imports from
lnp2pBot where they have 347 trades, 4.87 average, 3 years:

| Field | Before | After |
|---|---|---|
| `total_reviews` | 2 | 202 |
| `total_rating` | 4.5 | 4.5 |
| `since` | 10 days ago | 730 days ago |

Counterparties see "4.5 · 202 reviews · trading for 2 years".

### 6.1 Publish `since` instead of `days`

`days` is derived at publish time, so it is stale on any event that lives on
relays for a while, and it is awkward to merge. The underlying datum is a
date, so the public field should be one:

```json
{"total_reviews": 202, "total_rating": 4.5, "since": 1693526400}
```

`since` is a Unix timestamp **truncated to the start of its UTC day**
(`created_at - created_at % 86400`). Second precision would be a unique
fingerprint for the user, and the rating tag already travels on every order
of the same user, so it would make trade pubkeys perfectly correlatable.
Day precision carries exactly the information `days` carries today.

Clients compute the age at display time. The merge rule in section 6 becomes
`since = min(since_local, since_seed)`. The daemon publishes both `days` and
`since` for one deprecation window, in the order rating tag and in the
kind 38384 rating event, then drops `days`.

## 7. Mostro-to-Mostro specifics

- **Only native reputation is exportable.** When a Mostro acts as issuer it
  computes the cell from `total_reviews - seeded_reviews` and the native
  rating derived from `seeded_rating_sum`. Seeds never travel twice, which
  rules out A→B→A doubling and A→B→C chains. A user who wants A and C on B
  redeems one token from each; the destination records each issuer once per
  identity.
- **Fresh identity per instance.** The mobile app uses one identity key for
  every instance by default, so A and B can already link a user. Because the
  token binds to the *destination* pubkey, which the issuer never sees, the
  app can offer "use a new identity on this Mostro" and carry reputation over
  without the two identities being linkable. Without blinding this option
  would not exist.
- **Reputation is not publicly queryable by identity.** Mostro's rating events
  are keyed by trade pubkey, so a destination cannot simply read the source's
  relays; an issuer attestation is required even between Mostros.
- **Trust list.** Each instance configures accepted issuers. The reference
  instance ships with the lnp2pBot issuer pubkey enabled.

```toml
[reputation_import]
enabled = true
accepted_epochs = 2
issuers = [
  "<lnp2pbot issuer pubkey>",
  "<another mostro daemon pubkey>",
]
```

## 8. Multi-destination, Sybil and re-issuance

- **One token per source account, bound to one destination identity,
  redeemable on any number of Mostros.** Restricting the number of
  destinations is unenforceable without a shared ledger, and issuing
  per-destination tokens would tell the issuer which instances the user uses.
  Since reputation is per-instance, redeeming on N instances grants exactly
  what the user had, once each.
- **Sybil on one instance** (several identities sharing one reputation) is
  blocked by the identity binding plus the once-only issuance flag.
- **No re-issuance in v1.** A lost seed means the token cannot be re-obtained.
  Re-issuance would let one person hold two reputable identities on the same
  instance. An admin override for support cases is acceptable; a public
  re-issuance path is not (*open decision*).
- **Selling reputation** cannot be prevented cryptographically, since the
  issuer signs a pubkey it does not see. It is bounded by eligibility, by the
  one-token rule, and by being equivalent to selling the account.

## 9. Implementation plan

Ordered by dependency. Each item is one pull request, small enough to be
reviewed in full by a human and by the automated reviewers (CodeRabbit,
Codex). A PR never mixes a protocol type change with behaviour, nor a
migration with a handler. Every PR carries its own tests and lands green on
its own; nothing in a later phase starts until the release it depends on is
published.

Repositories: `protocol` (spec), `mostro-core` (shared types, crates.io),
`mostro` (daemon), `mobile` (app), `lnp2pbot/bot`.

### Phase 0: specification

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 0.1 | protocol | Add `since` to the `rating` tag and to the kind 38384 event, day-truncated Unix timestamp. Mark `days` deprecated with a removal version. | Spec merged, example events updated. |
| 0.2 | protocol | Reputation band grid as a protocol constant: the three dimensions, cuts, floors, cell id string format (`trades:200+\|rating:4.5+\|age:24m+`), K-anonymity merge rule. | Open decisions 1-3 closed and written down. |
| 0.3 | protocol | Issuer keyset event: kind number, `d` tag, `epoch` tag, content schema, accepted-epochs rule. | Kind number reserved. |
| 0.4 | protocol | Actions `export-reputation`, `export-reputation-response`, `import-reputation`, `reputation-imported`; payload schemas; new `cant-do` reasons; the redemption checks in section 5.3 as normative text. | Spec merged. |
| 0.5 | protocol | Test vectors: keyset, `B_`/`C_`/`C` for a fixed `r` and `k`, DLEQ, a valid token, and a table of invalid tokens (wrong cell, wrong identity, expired epoch). | Vectors file committed; every implementation below tests against it. |

### Phase 1: `since` rollout

Independent of the migration and worth shipping first.

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 1.1 | mostro-core | `Rating` gains `since: Option<u64>`; `to_tags()` emits `since` when set. Pure type change with unit tests. | Released as a patch version. |
| 1.2 | mostro | Bump core. `create_rating_tag` emits `days` **and** `since`; `rate_user` pushes a `since` tag next to `days`; one shared `day_truncate(created_at)` helper. Unit tests on both emitters. | Both events carry both fields on a local relay. |
| 1.3 | mobile | `Rating` model parses `since`, falls back to `days` when absent; age is computed at display time from `since`. Tests for both shapes. | Old and new daemons render the same age. |
| 1.4 | mostro | Remove `days` after the deprecation window (open decision: one minor release). | Removed with a changelog entry. |

### Phase 2: shared types in mostro-core

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 2.1 | mostro-core | Module `reputation::bands`: `TradesBand`, `RatingBand`, `AgeBand` enums with cuts and floors, `Cell { trades, rating, age }`, `Cell::from_stats(trades, avg_rating, since, now)`, `Cell::id()` / `parse()`. Pure, exhaustively tested against 0.2. | Round-trips every cell id. |
| 2.2 | mostro-core | `ReputationKeyset` (parse/build the event from 0.3, epoch handling) and `ReputationToken { issuer, epoch, cell, secret, c }` with serde and shape validation. No crypto. | Parses the 0.5 vectors. |
| 2.3 | mostro-core | `Action` variants `ExportReputation`, `ExportReputationResponse`, `ImportReputation`, `ReputationImported`; `Payload` variants `BlindedReputationRequest { b }`, `BlindedReputationResponse { c, cell, dleq }`, `ReputationToken`; `CantDoReason` variants `UntrustedIssuer`, `InvalidReputationToken`, `ReputationAlreadyRedeemed`, `ReputationIdentityMismatch`, `ExpiredKeysetEpoch`, `NotEligibleForExport`, `ReputationAlreadyExported`. Serde round-trip tests. | Released as a minor version. |

### Phase 3: mostrod as destination (import)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 3.1 | mostro | Settings section `[reputation_import]` (`enabled`, `issuers`, `accepted_epochs`) with parsing, defaults and validation. No behaviour. | Bad config is rejected at startup with a clear error. |
| 3.2 | mostro | Migration: `users` gains `seeded_reviews`, `seeded_rating_sum`, `reputation_exported_at`; new table `redeemed_reputation_tokens(secret_hash PK, issuer, identity_pubkey, cell, redeemed_at)` with a unique index on `(issuer, identity_pubkey)`. `db.rs` accessors with tests. | Migration applies on an existing database. |
| 3.3 | mostro | Crypto module `reputation::verify`: `hash_to_curve`, unblinded-signature verification and DLEQ verification on top of `cdk`. Tested against the 0.5 vectors, including every invalid case. | No daemon wiring yet. |
| 3.4 | mostro | Keyset fetcher: fetch and cache the issuer keyset event per trusted issuer, refresh on epoch change, reject unknown epochs. Tested with the `local-relay` feature. | Cache survives a relay outage. |
| 3.5 | mostro | Pure merge: `apply_seed(&mut User, &Cell, now)` implementing section 6 (trades add, weighted rating, `since` min, seeded counters). Property tests: never decreases `total_reviews`, never moves `created_at` forward, idempotent per issuer. | No I/O in the function. |
| 3.6 | mostro | Handler `import_reputation_action`: routing in `app.rs`, checks 1-5 of section 5.3 in one transaction, reply `reputation-imported` or `cant-do`. Integration test end to end with a fixture issuer. | A second redemption of the same token is rejected. |
| 3.7 | mostro | Publish the updated kind 38384 rating event after a successful import. | Event visible on the local relay. |

### Phase 4: mostrod as issuer (export)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 4.1 | mostro | Settings `[reputation_export]` (`enabled`, `epoch_length`); per-cell key derivation from the daemon key and epoch (HKDF, deterministic, never stored). Unit tests: same inputs, same keys. | No publication yet. |
| 4.2 | mostro | Publish the keyset event at startup and on epoch rollover, with the K-anonymity merge from section 4 using cell populations from the database. | Event validates against 2.2. |
| 4.3 | mostro | Native stats: `native_stats(identity)` = completed orders for the identity, native rating from `seeded_rating_sum`, `since` from `created_at`. Query plus pure function, tested. | Seeded values are excluded. |
| 4.4 | mostro | Handler `export_reputation_action`: eligibility, once-only flag, blind signature with `cdk`, DLEQ, reply; sets `reputation_exported_at`. Integration test: export from instance A, import on instance B, both in-process. | Round trip passes on two local daemons. |

### Phase 5: mobile

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 5.1 | mobile | BDHKE client primitives in Dart: `hashToCurve`, blind, unblind, DLEQ verify, over secp256k1. Tested against the 0.5 vectors. | No UI, no services. |
| 5.2 | mobile | Models: keyset event parser, `ReputationToken`, band cell; `MostroMessage` support for the new actions and payloads. | Serde round-trip tests. |
| 5.3 | mobile | Storage: Sembast repository for tokens and for in-flight blinding state (`r`, `secret`, issuer) so an interrupted flow resumes. | Survives app restart in a test. |
| 5.4 | mobile | `MostroService` + notifier: `exportReputation(issuer)` and `importReputation(token)` flows against a Mostro issuer, with the random redemption delay. | Integration test against a local daemon from 4.4. |
| 5.5 | mobile | Telegram transport: open `t.me/lnp2pbot?start=migrate_<B_>`, receive the response through the app's deep link scheme, verify DLEQ, store token. | Manual test with the bot from phase 6. |
| 5.6 | mobile | Settings screen "Import reputation": issuer list from the selected node's trust list, status per issuer, localized strings in every `intl_*.arb`. | `flutter analyze` clean, gen-l10n reports no untranslated keys. |
| 5.7 | mobile | Optional: "use a new identity on this instance" when importing. | Separate PR, can slip. |

### Phase 6: lnp2pBot as issuer

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 6.1 | bot | `REPUTATION_ISSUER_SK` in `.env-sample` and config validation; per-cell key derivation (same HKDF scheme as 4.1); `@cashu/crypto` dependency. Unit tests on derivation. | No command yet. |
| 6.2 | bot | Publish the keyset event through the existing `nostr` module at startup, with the K-anonymity merge computed from Mongo. | Event validates against 2.2. |
| 6.3 | bot | `User.reputation_exported_at`; `computeCell(user)` and `isEligible(user)` in `util/` using `trades_completed`, `total_rating`, `disputes`, `banned`, `created_at`. Tests on band edges. | Pure functions only. |
| 6.4 | bot | `/start migrate_<B_>` handler: parse, eligibility, once-only, blind sign, DLEQ, reply with the app deep link; error messages in every locale YAML. Tests with a mocked user. | Round trip with 5.5. |

### Phase 7: rollout

1. Deploy 1.2 and 1.3 first; wait one release before 1.4.
2. Deploy phase 3 on the reference instance with an empty issuer list.
3. Deploy phase 6 on the bot and phase 4 on the reference instance; add both keys to the trust list.
4. Ship the app with phases 5.1-5.6.
5. Document the operator side (`docs/REPUTATION_PORTABILITY.md`, settings template) and the user side (mobile in-app help).

## 10. Open decisions

1. Band cuts for the three dimensions (section 4).
2. `K` minimum cell population (default 50).
3. Eligibility minimums (default 10 trades, disputes lost < 10%).
4. Keyset event kind number and epoch length (default yearly).
5. Admin override for re-issuance: yes or no.
6. Length of the `days` deprecation window before PR 1.4 (default one minor release).
