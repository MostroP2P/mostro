# Reputation Portability

Design proposal for letting a user carry reputation earned in one trading venue
(lnp2pBot, or another Mostro instance) into a Mostro instance, as the exact
figures they earned there.

Status: **proposal**. Values marked *open decision* are defaults to be
confirmed before implementation.

## 1. Goals

1. A user with reputation on lnp2pBot can claim it on a Mostro instance.
2. The same mechanism lets a user copy reputation from one Mostro to another.
3. What travels is the **real** reputation — ratings received, average rating,
   and the date of the first trade — not an approximation of it.
4. Imported reputation is merged into the destination's ordinary reputation
   fields, so a counterparty sees one rating, one review count and one age, and
   never needs to know where it came from.
5. The destination operator decides which issuers to trust; nothing else is
   configurable per instance.

## 2. Non-goals

- Moving reputation. Portability is a **copy**: the source keeps it.
- Unlinkability. Importing tells the destination which source account the
  reputation came from, and tells the source which destination identity it was
  issued to (section 3).
- Users in `full_privacy` mode. They have no identity-bound reputation by
  design, so there is nothing to import into.

## 3. Threat model

Importing is **opt-in**, and a user who asks for it accepts that the two
identities become linkable:

- the issuer learns the destination identity pubkey it signs for;
- the destination operator learns the source account the attestation names;
- anyone who knows the user's figures on the source can recognise them, merged,
  on the destination.

The last point costs less than it sounds. A Mostro already publishes a user's
exact `total_reviews` and `total_rating` in the `rating` tag of every order
they make, so the figures are public on the source to begin with. A user who
does not want the two identities linked simply does not import.

An earlier version of this proposal kept the identities unlinkable by
carrying reputation as coarse bands under blind signatures. It is dropped:
the bands cost the user most of what they earned (a 4.9 average imported as
4.5, 214 ratings as 200), they needed a K-anonymity merge, per-cell keysets and
a two-round blind-signing protocol to work at all, and the unlinkability they
bought was only probabilistic against timing analysis.

What the design must still guarantee:

| Property | How |
|---|---|
| Only a trusted issuer can mint reputation | Attestation signed by the issuer key; destination keeps a trust list (section 5.3) |
| An attestation serves one destination identity | It names that identity; redemption requires the transport's identity proof to match (section 5.3) |
| One source account seeds one destination identity | The issuer binds the source account to the first destination identity it exports to (section 5.2) |
| A source account is counted once per destination | The destination records `(issuer, subject)` and refuses a second import (section 5.3) |
| A seed never travels twice | A Mostro issuer exports native reputation only (section 7) |
| Stale figures cannot be imported later | Attestations expire (section 5.1) |

## 4. What travels

Three figures, all of them the user's own native reputation on the source:

| Field | Meaning | Mostro issuer | lnp2pBot issuer |
|---|---|---|---|
| `reviews` | Ratings received | `total_reviews - seeded_reviews` | `total_reviews` |
| `rating` | Average of those ratings | `native_rating_sum / native_reviews` | `total_rating` |
| `since` | Day-truncated date of the first trade | `day_truncate(native_created_at)` | `day_truncate(created_at)` |

- **Ratings received, not completed trades.** On Mostro `total_reviews` only
  increments when a counterparty actually submits a rating, and it is also the
  weight of the running average. Seeding it from a trade count would publish a
  review count the user never earned and give a single rating the weight of
  hundreds. Completed trades are not carried.
- **`since` is day-truncated** with the same rule as the public `rating` tag
  (section 6.1), so the attestation carries no more precision than is already
  public.
- **Eligibility** (*open decision*): not banned, and at least 1 rating
  received. Below that there is nothing to import, and the issuer refuses.

## 5. The attestation

### 5.1 Format

An attestation is a **Nostr event signed by the issuer** and never published
to relays: it travels inside the `reputation-exported` reply and the
`import-reputation` request. Using a Nostr event means every implementation
already has the canonical serialisation, the event id and BIP-340
verification, and nothing new has to be specified byte by byte.

```json
{
  "id": "<event id>",
  "pubkey": "<issuer key>",
  "created_at": 1790899200,
  "kind": 38388,
  "tags": [
    ["p", "<destination identity pubkey>"],
    ["subject", "<source account id>"],
    ["reviews", "214"],
    ["rating", "4.87"],
    ["since", "1696204800"],
    ["expiration", "1791504000"],
    ["z", "reputation-attestation"]
  ],
  "content": "",
  "sig": "<issuer signature>"
}
```

| Tag | Rule |
|---|---|
| `p` | 32-byte x-only destination identity pubkey, lowercase hex |
| `subject` | Stable, opaque id of the source account **within this issuer**. A Mostro issuer uses the user's identity pubkey (hex). lnp2pBot uses its internal user id, not the Telegram id (*open decision*): the destination needs a dedup key, not the Telegram account |
| `reviews` | Decimal integer, `≥ 1` |
| `rating` | Decimal string with at most two fractional digits, in `[1, 5]` |
| `since` | Unix seconds, a multiple of 86400, not in the future |
| `expiration` | [NIP-40](https://github.com/nostr-protocol/nips/blob/master/40.md) Unix seconds; `created_at` + the attestation lifetime (*open decision*, default 7 days) |
| `z` | `reputation-attestation` |

Each tag appears exactly once. The kind (*open decision*, default `38388`)
gives the signature its own domain: an issuer key that also signs other events
can never have one of those accepted as an attestation.

### 5.2 Issuance (export)

One request and one reply; there is no session.

1. The client asks the issuer for an attestation for a destination identity
   pubkey.
2. The issuer looks up the user and checks eligibility (section 4).
3. **Binding.** The first export records the destination identity in
   `reputation_exported_to`. A later export is allowed only to that same
   identity, and is how a user re-obtains a lost attestation or a fresher
   one; an export to any other identity is refused. This is what stops one
   source account from seeding several identities. An admin override for
   support cases is acceptable (*open decision*); a public rebinding path is
   not.
4. The issuer signs the attestation with its issuer key and returns it.
5. The client verifies the signature and shows the user the figures before
   importing.

Transport differs per issuer:

- **lnp2pBot**: a Telegram deep link `t.me/lnp2pbot?start=rep_<pubkey>`,
  where `<pubkey>` is the destination identity as 43 characters of unpadded
  base64url — Telegram caps the start parameter at 64 characters, which a hex
  key with a prefix would exceed. The bot replies with a button opening the
  app's deep link with the attestation; on desktop and web the user pastes it.
- **Mostro**: an `export-reputation` message over the daemon's ordinary
  protocol transport (v2: NIP-44 `kind: 14`, authored by a trade key with the
  identity proof inside the ciphertext); the daemon replies
  `reputation-exported` with the attestation.

### 5.3 Redemption (import)

A new action `import-reputation` carrying the attestation, sent over the
daemon's ordinary protocol transport. The destination daemon:

1. Parses the event and verifies its id and signature.
2. Checks the kind, the `z` tag, and every tag rule in section 5.1.
3. Checks `pubkey` is in its trusted issuer list.
4. Checks `created_at` is not in the future (beyond a small clock skew) and
   `expiration` has not passed.
5. Checks the `p` tag equals `UnwrappedMessage.identity` — the identity the
   transport *proved*, not one the sender claims. On protocol v2 that proof is
   `identity_sig`, a domain-tagged signature bound to the trade pubkey
   authoring the event, so it cannot be grafted from another sender.
6. Checks that neither `(issuer, subject)` nor `(issuer, identity)` has been
   imported before on this instance. The first stops a source account from
   seeding two identities here even if an issuer broke its own binding; the
   second stops an identity from importing two accounts from one issuer.
7. Records the import and applies it to the user row (section 6) in the same
   transaction.

Replies: `reputation-imported` on success; `cant-do` with one of the new
`CantDoReason` variants on failure (section 9, PR 2.2). `CantDoReason` is the
one enum in core carrying `#[serde(other)]`, so an old client degrades a new
reason to `Unknown` instead of failing. `Action` and `Payload` have no such
fallback — see section 9 for why that is still safe.

The issuer's identity is **one key**: it signs attestations, and it is what a
destination lists as trusted. For lnp2pBot that key is
`REPUTATION_ISSUER_SK`, kept separate from the bot's existing `NOSTR_SK` so
issuance can be rotated or revoked without disturbing the bot's other Nostr
activity. A Mostro uses its daemon key.

## 6. Merging on the destination

Each figure has its own merge rule, because they measure different things:

- **Ratings received add up.** A rating on A and a rating on B are distinct
  reviews by distinct counterparties.
- **Rating is a weighted average**, weighted by review count per origin. It is
  never summed and never maxed. Because the imported average is the real one,
  importing never drags a user's rating towards an arbitrary floor.
- **Account age is a minimum date, never a sum.** "Since when does this person
  trade?" is a single date, the oldest one that can be proven. Time passes in
  parallel on every venue, so adding day counts would count the same month
  twice: a user who opened orders on A and B on the same 10 August has 30 days
  on both a month later, and importing A into B leaves B at 30, not 60.

Imports are applied to the user's existing values; nothing is overwritten.
The math lives next to `User::update_rating` in mostro-core, which already
owns the running-average formula (first vote weighted 1/2, then incremental
mean). A user with imported reputation has `total_reviews > 1` from the start,
so the first-vote damping no longer applies to them; that is the intended
anchor effect.

| `users` column | Effect |
|---|---|
| `total_reviews` | `+= reviews` |
| `total_rating` | `= (total_rating × total_reviews + rating × reviews) / (total_reviews + reviews)`, using the values before the import |
| `created_at` | `= min(created_at, since)` — the displayed age |
| `native_created_at` (new) | **unchanged**; the account's own creation date |
| `min_rating`, `max_rating`, `last_rating` | unchanged if non-zero, otherwise set to `round(rating)` |
| `seeded_reviews` (new) | `+= reviews`, internal only |
| `seeded_rating_sum` (new) | `+= rating × reviews`, internal only |
| `native_rating_sum` (new) | **unchanged**; incremented by `update_rating` on native reviews only, internal only |

Each import is also kept as a row of `reputation_imports` (issuer, subject,
identity, the three figures, the attestation id and the import date): it is
what step 6 of section 5.3 checks, and it keeps every seed attributable.

`native_created_at` is set once, when the row is created, and no import ever
moves it. Without it age would leak across hops: A→B moves B's `created_at`
back, and B exporting to C would then present A's date as its own. Reviews
and rating are protected the same way by `seeded_reviews`,
`seeded_rating_sum` and `native_rating_sum`; age needs its own field because
the merge is a minimum rather than a subtraction.

The public `rating` tag on order events keeps its shape (see 6.1). No marker
distinguishing imported from native reputation is published.

Worked example. A user with 10 days on Mostro and 2 ratings at 4.5 imports
from lnp2pBot, where they have 214 ratings received at 4.87 and have traded
since 3 years ago:

| Field | Before | After |
|---|---|---|
| `total_reviews` | 2 | 216 |
| `total_rating` | 4.5 | 4.87 (`(2 × 4.5 + 214 × 4.87) / 216 = 4.866…`) |
| `since` | 10 days ago | 3 years ago |

Counterparties see "4.87 · 216 reviews · trading for 3 years".

### 6.1 Publish `since` instead of `days`

`days` is derived at publish time, so it is stale on any event that lives on
relays for a while, and it is awkward to merge. The underlying datum is a
date, so the public field should be one:

```json
{"total_reviews": 216, "total_rating": 4.87, "since": 1696204800}
```

`since` is a Unix timestamp **truncated to the start of its UTC day**
(`created_at - created_at % 86400`). Second precision would be a unique
fingerprint for the user, and the rating tag already travels on every order
of the same user, so it would make trade pubkeys perfectly correlatable.
Day precision carries exactly the information `days` carries today.

Clients compute the age at display time. The merge rule in section 6 becomes
`since = min(since_local, since_imported)`.

The day count is exposed in **three** places today, and all three change:

| Site | Today | Owner |
|---|---|---|
| `rating` tag on order events | `days` inside the JSON built by `create_rating_tag` | mostro |
| kind 38384 rating event | `days` tag pushed by `rate_user` next to `Rating::to_tags()` | mostro; `Rating` itself has no date field |
| `Peer` payload sent to the counterparty | `UserInfo.operating_days`, filled in `util.rs` | mostro-core type, mostro fills it |

The daemon publishes both `days` and `since` for one deprecation window at
all three sites, then drops `days`. `Rating::from_tags` ignores unknown keys
and `UserInfo` gains the field as `Option` with a serde default, so old and
new peers interoperate during the window.

## 7. Mostro-to-Mostro specifics

- **Only native reputation is exportable.** When a Mostro acts as issuer it
  takes the figures from `User::native_stats`, which reads
  `native_created_at` rather than the moved-back `created_at` and undoes the
  import arithmetic of section 6 exactly:

  ```text
  native_reviews = total_reviews - seeded_reviews
  native_rating  = native_rating_sum / native_reviews
  ```

  where `native_rating_sum` is a new `users` column that `update_rating`
  increments by the raw rating on every native review, and that
  `apply_reputation_import` never touches. Reversing `total_rating` instead is
  not exact: the running mean is stored in `f64`, and the first-vote 1/2
  weighting means `total_rating × total_reviews` is not the sum of ratings
  for a user with a single native review. A user with no native reviews is
  not eligible to export. Imports never travel twice, which rules out A→B→A
  doubling and A→B→C chains. A user who wants A and C on B imports one
  attestation from each.
- **One identity across instances.** An attestation names the destination
  identity pubkey, and redemption requires that key to sign, so a single
  attestation serves one identity — on as many instances as trust the issuer.
  The app already uses one identity key for every instance, so this is the
  normal case.
- **Reputation is not publicly queryable by identity.** Mostro's rating events
  are keyed by trade pubkey, so a destination cannot simply read the source's
  relays; an issuer attestation is required even between Mostros.
- **Trust list.** Each instance configures accepted issuers. The reference
  instance ships with the lnp2pBot issuer pubkey enabled.

```toml
[reputation_import]
enabled = true
issuers = [
  "<lnp2pbot issuer pubkey>",
  "<another mostro daemon pubkey>",
]
```

## 8. Multi-destination, Sybil and re-issuance

- **One source account, one destination identity, any number of Mostros.**
  The binding is to the identity, not to the instance: the same identity can
  redeem its attestation on every instance that trusts the issuer, and since
  reputation is per-instance, that grants exactly what the user had, once
  each. Restricting the number of destinations is unenforceable without a
  shared ledger anyway.
- **Sybil on one instance** (several identities sharing one reputation) is
  blocked twice: by the issuer's binding (section 5.2) and by the
  destination's `(issuer, subject)` check (section 5.3).
- **Re-issuance is free to the bound identity.** A lost attestation, or one
  that expired before it was used, is simply requested again. Rebinding to a
  different identity is an admin decision, not a user action.
- **Refreshing figures** after a first import — replacing an earlier import
  from the same source with newer figures — is not part of v1: a second
  import from the same `(issuer, subject)` is refused (*open decision*). The
  per-import rows of section 6 make it straightforward to add later, because
  the earlier contribution is known exactly and can be swapped out.
- **Selling reputation** cannot be prevented, only bounded: by eligibility, by
  the one-identity binding, and by being equivalent to selling the account.

## 9. Implementation plan

Ordered by dependency. Each item is one pull request, small enough to be
reviewed in full by a human and by the automated reviewers (CodeRabbit,
Codex). A PR never mixes a protocol type change with behaviour, nor a
migration with a handler. Every PR carries its own tests and lands green on
its own; nothing in a later phase starts until the release it depends on is
published.

Repositories: `protocol` (spec), `mostro-core` (shared types, crates.io),
`mostro` (daemon), `mobile` (app 1.x, pure Dart), `app` (app 2.x, Flutter UI
over a Rust core through flutter_rust_bridge, already on mostro-core and
nostr-sdk 0.45), `lnp2pbot/bot`.

Both apps must support the migration. No new cryptography is involved:
building and verifying the attestation is ordinary Nostr event signing, which
every codebase here already does.

**Protocol compatibility.** `PROTOCOL_VER` stays at 2 and the new actions do
not gate on it. In core only `CantDoReason` carries `#[serde(other)]`;
`Action` and `Payload` would fail to deserialise an unknown variant. That is
safe here because these messages are strictly request/response over targeted
encrypted direct messages: `export-reputation` and `import-reputation` are only ever sent by
a client that implements them, and `reputation-exported` / `reputation-imported`
only ever come back to the client that asked. No new variant is broadcast, so
no old client is ever handed one. PR 2.2 carries a test that pins this
reasoning.

### Phase 0: specification

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 0.1 | protocol | Add `since` to the `rating` tag and to the kind 38384 event, day-truncated Unix timestamp. Mark `days` deprecated with a removal version. | Spec merged, example events updated. |
| 0.2 | protocol | Reputation attestation event: kind number reserved, tag schema and rules from section 5.1, the redemption checks of section 5.3 as normative text. | Kind number reserved. |
| 0.3 | protocol | Actions `export-reputation`, `reputation-exported`, `import-reputation`, `reputation-imported` — those four kebab-case strings are the wire discriminators, used verbatim in the schema, the core enum serialization, the compatibility note and the vectors. Payload schemas; new `cant-do` reasons. | Spec merged; one name per action across every document. |
| 0.4 | protocol | Test vectors: an issuer secret key, a valid attestation signed with it and its event id, the merge of section 6 on a fixed user row, and a table of invalid attestations (wrong kind, missing or repeated tag, `rating` out of range or with three decimals, `since` not day-aligned, expired, future `created_at`, `p` not matching the identity, mauled signature, untrusted issuer). | Vectors file committed; every implementation below tests against it. |

### Phase 1: `since` rollout

Independent of the migration and worth shipping first.

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 1.1 | mostro-core | `src/rating.rs`: `Rating` gains `since: Option<u64>` (serde default); `to_tags()` emits a `since` tag when set, `from_tags()` parses it; `Rating::new` keeps its signature and a `with_since(u64)` builder is added so no caller breaks. `src/user.rs`: `UserInfo` gains `since: Option<u64>` (serde default). Unit tests for both round trips. | Released (shipped in 0.15.0). |
| 1.2 | mostro | Bump core. One helper `day_truncate(created_at) -> u64` in `util.rs`. `create_rating_tag` (`nip33.rs`) emits `days` **and** `since`; `rate_user.rs` builds the `Rating` with `.with_since()` and keeps pushing the `days` tag; the `UserInfo` built in `util.rs` fills `since`. Unit tests on the three emitters. | All three sites carry both fields on a local relay. |
| 1.3 | mobile | `data/models/rating.dart` parses `since` and falls back to `days`; `data/models/user_info.dart` parses `since` and falls back to `operating_days`; age is computed at display time. Tests for both shapes. | Old and new daemons render the same age. |
| 1.3b | app | Rust core: `nostr/order_events.rs::parse_rating_tag` reads `since` and falls back to `days`; `mostro/status.rs` maps `UserInfo.since` with fallback to `operating_days`. Bridge exposes `since` and the Dart side (`peer_reputation_card.dart`, trade detail) computes the age at display time. Tests for both shapes. | Old and new daemons render the same age. |
| 1.4 | mostro | Remove `days` and `operating_days` emission after the deprecation window (open decision 5). `UserInfo.operating_days` removal in core is a **minor** bump and goes with PR 2.2. | Removed with a changelog entry. |

### Phase 2: shared types in mostro-core

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 2.1 | mostro-core | Module `reputation`: `ReputationAttestation { issuer, destination, subject, reviews, rating, since, created_at, expiration }`, `build(&Keys, …) -> Event` and `parse(&Event, now) -> Result<ReputationAttestation, _>` applying every rule of section 5.1 plus signature, kind and time checks. The trust list and the identity match stay with the caller. Pure. | Parses and rejects the 0.4 vectors exactly. |
| 2.2 | mostro-core | `src/message.rs`: `Action` variants `ExportReputation`, `ReputationExported`, `ImportReputation`, `ReputationImported` (past participle for daemon replies, matching `Released` / `Canceled`); `Payload` variants `ReputationExportRequest { destination }` and `ReputationAttestation(String)` carrying the event JSON. `src/error.rs`: `CantDoReason` variants `UntrustedReputationIssuer`, `InvalidReputationAttestation`, `ExpiredReputationAttestation`, `ReputationIdentityMismatch`, `ReputationAlreadyImported`, `NotEligibleForReputationExport`, `ReputationBoundToOtherIdentity`, inserted before `Unknown`. Serde round-trip tests, plus a test asserting an old client never receives these variants (`PROTOCOL_VER` stays 2, see the compatibility note above). | Part of the **minor** release. |
| 2.3 | mostro-core | `src/user.rs`: `User` gains `seeded_reviews: i64`, `seeded_rating_sum: f64`, `native_rating_sum: f64`, `native_created_at: Option<i64>`, `reputation_exported_to: Option<String>` and `reputation_exported_at: Option<i64>` (day-truncated), each with `#[sqlx(default)]` and `#[serde(default)]` so a daemon on an un-migrated database still deserialises `SELECT *`. `native_created_at` is an `Option` on purpose: `#[sqlx(default)]` on a plain `i64` would read a missing column as `0`, and `native_stats` would then place every legacy user in 1970. `User::new` sets it to `Some(created_at)`; every reader goes through `User::native_created_at()`, which falls back to `created_at` when the column is absent. | Test with a row that lacks the columns and assert the fallback equals `created_at`, never `0`. |
| 2.4 | mostro-core | `src/user.rs`: `User::apply_reputation_import(&mut self, &ReputationAttestation)` implementing section 6 next to `update_rating`, plus `User::native_stats(&self) -> (reviews, rating, since)` implementing the section 7 formula. `update_rating` gains `native_rating_sum += rating`; `apply_reputation_import` leaves it alone. Unit tests: the 0.4 merge vector; one native review (rating 5 → native rating 5.0 even though `total_rating` is 2.5 by the first-vote rule); several imports from different issuers; native reviews before and after an import. Property tests: never decreases `total_reviews`, never moves `created_at` forward, never touches `native_created_at` or `native_rating_sum`, and an A→B→C chain exports the same native stats B had before importing from A. | No I/O; released together with 2.1-2.3. |

### Phase 3: mostrod as destination (import)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 3.1 | mostro | Settings section `[reputation_import]` (`enabled`, `issuers`) with parsing, defaults and validation. No behaviour. | Bad config is rejected at startup with a clear error. |
| 3.2 | mostro | Bump core. Migration: `users` gains `seeded_reviews`, `seeded_rating_sum`, `native_rating_sum` (backfilled as `total_rating * total_reviews`; no per-review history exists to replay, and this makes a legacy row's native rating equal its displayed `total_rating` exactly), `native_created_at` (backfilled from `created_at`), `reputation_exported_to` and `reputation_exported_at`; new table `reputation_imports(attestation_id PK, issuer, subject, identity_pubkey, reviews, rating, since, imported_at)` with unique indexes on `(issuer, subject)` and `(issuer, identity_pubkey)`. `db.rs` accessors with tests. | Migration applies on an existing database; the `users` insert in `db.rs` binds the new columns. |
| 3.3 | mostro | Handler `src/app/import_reputation.rs`: routing in `app.rs`, `ReputationAttestation::parse` from core, then the trust list, identity and uniqueness checks of section 5.3 and `User::apply_reputation_import` in one transaction; replies `reputation-imported` or `cant-do`. Integration test end-to-end with a fixture issuer key. | A second import of the same source is rejected. |
| 3.4 | mostro | Publish the updated kind 38384 rating event after a successful import, reusing `update_user_rating_event`. | Event visible on the local relay. |

### Phase 4: mostrod as issuer (export)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 4.1 | mostro | Settings `[reputation_export]` (`enabled`, attestation lifetime). Eligibility from `User::native_stats` and `is_banned`. Tested. | Imported values never reach an attestation. |
| 4.2 | mostro | Handler `export_reputation_action`: eligibility, the binding of section 5.2 (`reputation_exported_to`, `reputation_exported_at`), sign with the daemon key, reply `reputation-exported`. Integration test: export from instance A, import on instance B, both in-process. | Round trip passes on two local daemons; an export to a second identity is refused. |

### Phase 5: mobile (app 1.x, `mobile`)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 5.1 | mobile | Models: attestation parsing and validation against the 0.4 vectors; `MostroMessage` support for the new actions and payloads. | Serde round-trip tests; every 0.4 vector accepted or rejected as specified. |
| 5.2 | mobile | `MostroService` + notifier: `exportReputation(issuer)` and `importReputation(attestation)` flows against a Mostro issuer; the attestation is stored until imported. | Integration test against a local daemon from 4.2. |
| 5.3 | mobile | Telegram transport: the `start=rep_…` deep link, receiving the attestation through the app deep link, verification, and a confirmation screen showing the figures before import. | Manual test with the bot from phase 6. |
| 5.4 | mobile | Settings screen "Import reputation": issuer list from the selected node's trust list, status per issuer, localized strings in every `intl_*.arb`. | `flutter analyze` clean, gen-l10n reports no untranslated keys. |

### Phase 5b: app 2.x (`app`)

Runs in parallel with phase 5; shares the UI copy and the localized strings.

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 5b.1 | app | Rust core: bump mostro-core so `ReputationAttestation` and the new `Action` / `Payload` / `CantDoReason` variants come from core; `export_reputation(issuer)` and `import_reputation(attestation)` through the existing message queue, the attestation persisted until imported. Bridge functions exposed through flutter_rust_bridge (generated, never hand-written). | Integration test against the local daemon from 4.2. |
| 5b.2 | app | Telegram transport: the deep link on mobile and a paste field on desktop and web, handing the attestation to the Rust core for validation. | Manual test with the bot from phase 6. |
| 5b.3 | app | Dart UI: settings screen "Import reputation" with the confirmation step, reusing the 1.x copy, localized in every ARB under `lib/l10n/`. | `flutter analyze` clean, no untranslated keys. |

### Phase 6: lnp2pBot as issuer

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 6.1 | bot | `REPUTATION_ISSUER_SK` in `.env-sample` and config validation — the bot's issuer identity, deliberately not `NOSTR_SK`. `User.reputation_exported_to` and `reputation_exported_at` (day-truncated); `isEligible(user)` in `util/`. Tests. | Pure functions only. |
| 6.2 | bot | `/start rep_…` handler: decode the destination identity, eligibility, binding, sign the attestation through the existing `nostr` module, reply with the app deep link; error messages in every locale YAML. Tests against the 0.4 vectors and with a mocked user. | Round trip with 5.3. |

### Phase 7: rollout

1. Deploy 1.2, then ship 1.3 and 1.3b; wait one release before 1.4.
2. Deploy phase 3 on the reference instance with an empty issuer list.
3. Deploy phase 6 on the bot and phase 4 on the reference instance; add both keys to the trust list.
4. Ship the 1.x app with phases 5.1-5.4 and the 2.x app with phases 5b.1-5b.3.
5. Document the operator side (`docs/REPUTATION_PORTABILITY.md`, settings template) and the user side (mobile in-app help).

## 10. Open decisions

1. Attestation kind number (default `38388`).
2. Attestation lifetime (default 7 days).
3. Eligibility (default: not banned, at least 1 rating received).
4. lnp2pBot `subject`: internal user id or Telegram id (default internal id).
5. Length of the `days` deprecation window before PR 1.4 (default one minor release).
6. Admin override to rebind a source account to a different identity: yes or no.
7. Refreshing an earlier import with newer figures: in v1 or later (default later).

Closed during review:

- **Exact figures, not bands.** Reputation travels as the user's real figures;
  unlinkability is not a goal, because importing is opt-in and the figures are
  public on the source anyway. This drops the band grid, K-anonymity, the
  per-cell keysets and blind Schnorr signatures (section 3).
- **Source identity in the attestation.** The `subject` tag names the source
  account, so the destination can refuse a second import of it.
- **Merged, not marked.** Imported reputation is merged into the ordinary
  fields and no marker is published.
- **`PROTOCOL_VER`.** Stays at 2; the new actions do not gate on it (section 9).
- **Trades vs reviews.** Ratings received are carried; completed trades are not.
- **Native reputation only.** `native_created_at` and `native_rating_sum` keep
  an imported figure from being re-exported as native.
- **Issuer identity.** One key per issuer; for the bot that is
  `REPUTATION_ISSUER_SK`, not `NOSTR_SK`.
- **Action naming.** `reputation-exported` is the wire name for the export
  response, everywhere.
