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
   the source Mostro) with the destination identity pubkey **from the protocol
   data itself** — not even when the source and destination operators are the
   same person, or when the source database leaks. The guarantee is
   cryptographic on the data; it is only probabilistic against traffic
   analysis, and section 3 states the residual explicitly.
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

**Residual risk: timing correlation.** The cryptography makes the token
itself unlinkable, but it cannot hide *when* things happen. With few
migrations per day, a colluding pair can match "issued at 14:02" with
"redeemed at 14:05" and defeat goal 3 statistically. This is accepted, not
solved, and it is bounded rather than eliminated:

- The client waits a random delay before redeeming, drawn from a window wide
  enough that many issuances fall inside it (*open decision* 8, default 24h).
  The effective anonymity set is the number of migrations from the same
  issuer within that window, which is small on day one and grows with
  adoption.
- Issuers store no issuance timestamp finer than the per-user flag in
  section 6, so the correlation must be done live; it cannot be reconstructed
  later from the database alone.
- Nothing here helps the very first migrant. Anyone for whom this matters
  should wait until the issuer has processed a meaningful number of exports.

## 4. Reputation bands

Reputation is quantised into a cell of a three-dimensional grid. The grid is a
**protocol constant**: every issuer uses the same cuts, so a token means the
same thing wherever it is redeemed. Operators choose whom to trust, not what a
band means.

All intervals are **half-open**, `[low, high)`, so every value falls in
exactly one band and two implementations cannot disagree on a boundary. A
"month" is exactly 30 days of 86400 seconds; ages are computed from
`day_truncate(created_at)` (section 6.1) so the band does not flip mid-day.

| Dimension | Bands (*open decision*) | Floor | Seeds on the destination |
|---|---|---|---|
| Ratings received | `[1,10)`, `[10,50)`, `[50,200)`, `[200,∞)` | 1 / 10 / 50 / 200 | `total_reviews` += floor |
| Average rating | `[0,4.0)`, `[4.0,4.5)`, `[4.5,5.0]` | 0.0 / 4.0 / 4.5 | `total_rating` weighted with floor |
| Account age | `[0,6m)`, `[6m,24m)`, `[24m,∞)` | 0d / 180d / 720d | `created_at` moved back by floor |

The top rating band is closed at 5.0 because that is the maximum a rating can
take; every other band is half-open.

Rules:

- **The first dimension counts ratings received, not completed trades.** On
  Mostro `total_reviews` only increments when a counterparty actually submits
  a rating, and it is also the weight of the running average. Seeding it from
  a trade count would publish a review count the user never earned and give a
  single rating the weight of hundreds. On lnp2pBot the matching field is
  `total_reviews`; completed trades are used for eligibility only.
- Seeds always use the **floor** of the band. Nobody receives more than they
  earned; the destination shows a conservative lower bound.
- Cells are the leaves of the issuer keyset (section 5). An issuer must merge
  any cell holding fewer than `K` users (*open decision*, default 50) into an
  adjacent lower cell before publishing, so every cell is a real crowd. The
  merge is **deterministic and published**: a sparse cell steps down one band,
  trying dimensions in the fixed order ratings, age, rating, and repeats until
  the absorbing cell holds at least `K` or no lower band exists in any
  dimension, in which case it merges into the nearest non-empty lower cell.
  The resulting raw→effective map ships in the keyset, so a client can apply
  the same map and check the issuer followed it.
- Eligibility (*open decision*): at least 10 completed trades **and** at least
  1 rating received, not banned, and disputes lost below 10% of completed
  trades. Below eligibility the issuer refuses and there is nothing to
  migrate.

## 5. Cryptography: blind Schnorr signatures per cell

The token must be verified by the **destination**, which is not the party that
issued it. That rules out Cashu's BDHKE: there the mint verifies its own
tokens with the private key `k`, and given only `K = k·G` deciding whether
`C = k·Y` is the Decisional Diffie-Hellman problem, assumed hard on
secp256k1. A third party simply cannot check a BDHKE token.

The scheme is therefore **blind Schnorr on secp256k1**, which is blind at
issuance and *publicly* verifiable at redemption: anyone holding the issuer's
public key can check the signature. Each grid cell has its own keypair, so the
key that verifies a token is what states its band.

Concurrent blind Schnorr sessions are subject to the ROS attack, so issuers
use the **clause variant** (Fuchsbauer–Kiltz–Loss): the issuer opens two
nonce clauses, the client prepares a challenge for each, and the issuer
answers only one, chosen at random. Issuers additionally allow one open
session per user at a time.

Implementations need scalar and point arithmetic on secp256k1, not a Cashu
library: `k256` in mostro and in the 2.x app's Rust core, `@noble/curves` in
the bot, and a small Dart implementation in the 1.x app.

### 5.1 Canonical encoding

Everything hashed or transmitted has exactly one byte encoding, so the Rust,
JavaScript and Dart implementations cannot derive different challenges for the
same token. Every field is fixed length, which makes concatenation unambiguous
without extra framing.

| Element | Encoding |
|---|---|
| Curve points | 33-byte compressed SEC1 (`0x02`/`0x03` prefix) |
| Scalars | 32-byte big-endian, reduced mod `n`, rejected if zero |
| Tagged hash | `H_tag(x) = SHA256(SHA256(tag) ‖ SHA256(tag) ‖ x)`, as in BIP-340 |
| Challenge | `c = int(H_"mostro/reputation/challenge/v1"(R ‖ m)) mod n`, `R` compressed |
| `m` | `"repv1:"` (6 ASCII bytes) ‖ 32-byte x-only destination identity pubkey ‖ 32-byte random nonce — 70 bytes exactly |
| Token id | `H_"mostro/reputation/token/v1"(m)`, the primary key in `redeemed_reputation_tokens` |
| Cell id | UTF-8 `reviews:<band>\|rating:<band>\|age:<band>`, no spaces, bands spelled as in section 4 |

Points are compressed rather than x-only, and the challenge is a plain tagged
hash rather than BIP-340's: BIP-340 normalises `R` to even Y, and the client's
blinded `R_i = R'_i + α_i·G + β_i·P_cell` has unpredictable parity that the
signer cannot compensate for, so x-only encoding would break the blinding.

### 5.2 Issuer keyset

An issuer holds one secret scalar `x_cell` per grid cell and publishes the
public points `P_cell = x_cell·G` as a parameterised replaceable Nostr event
signed with the issuer's own key:

```text
kind: 30xxx (open decision)
tags: ["d", "reputation-keyset:2026"], ["epoch", "2026"]
content: {
  "cells":  {"reviews:200+|rating:4.5+|age:24m+": "<hex compressed P>", ...},
  "merges": {"reviews:200+|rating:0-4.0|age:24m+": "reviews:50-200|rating:0-4.0|age:24m+", ...}
}
```

`cells` holds only the **effective** cells, the ones that actually have a key.
`merges` is the published map from every raw cell that was folded away to the
cell that absorbed it, so the client can reproduce the issuer's choice instead
of having to trust it (section 5.3, step 3).

The epoch is part of the `d` tag, not only a separate tag. Addressable events
replace on `(kind, pubkey, d)`, so a fixed `d` would make each rotation
**delete the previous keyset** from relays and leave the destination unable to
verify tokens from the epoch it still accepts.

The issuer's identity is **one key**: it signs the keyset event, it is what a
destination lists as a trusted issuer, and it is the `issuer` field of every
token. For lnp2pBot that key is `REPUTATION_ISSUER_SK`, kept separate from the
bot's existing `NOSTR_SK` so reputation issuance can be rotated or revoked
without disturbing the bot's other Nostr activity. A Mostro uses its daemon
key. Keysets are rotated by **epoch** (yearly); a destination accepts
the current and previous epoch only, so stale reputation cannot be imported
years later and an issuer can retire keys.

### 5.3 Issuance (export)

The blinding runs on the user's device; the issuer never sees the message it
signs. Writing `m` for that message and `H` for the challenge hash:

1. Client builds `m = "repv1:" || destination_identity_pubkey || nonce`.
2. Issuer looks up the user, checks eligibility and the once-only flag, picks
   the cell, and opens the session by sending the cell id and two nonce
   points `R'_0 = k_0·G`, `R'_1 = k_1·G`.
3. **Client validates the cell against its own statistics.** The user knows
   their own review count, rating and account age, so the client computes the
   raw cell, applies the keyset's published `merges` map transitively, and
   aborts if the result differs from the cell the issuer named. Applying the
   map is what makes this compatible with K-anonymity merging: a user whose
   raw cell was folded away still validates, because the client folds it the
   same way. Without this check an adversarial issuer could tag a user by
   assigning a deliberately rare cell and recognise it at redemption; no
   proof about the signature itself would catch that, because the signature
   would be perfectly valid.
4. Client picks random `α_i`, `β_i` for each clause, computes
   `R_i = R'_i + α_i·G + β_i·P_cell` and `c'_i = H(R_i ‖ m) + β_i`, and sends
   both `c'_i`.
5. Issuer picks `b ∈ {0,1}` at random and returns `s'_b = k_b + c'_b·x_cell`
   with `b`. Answering only one clause is what defeats ROS.
6. Client unblinds `s = s'_b + α_b` and stores the token
   `{issuer, epoch, cell, m, R_b, s}`.
7. Client verifies `s·G == R_b + H(R_b ‖ m)·P_cell` against the published
   keyset **before** storing. This is the same check the destination will run,
   so a token that would be rejected later is caught immediately, and an
   issuer that signed with an off-keyset key is detected at once.

Transport differs per issuer:

- **lnp2pBot**: two round trips over the Telegram deep link
  (`t.me/lnp2pbot?start=migrate_...`), the bot answering into the app's deep
  link scheme. The bot sets `reputation_exported_at` on the user and stores
  nothing else.
- **Mostro**: `export-reputation` messages over the daemon's ordinary
  protocol transport (v2: NIP-44 `kind: 14`, authored by a trade key with the
  identity proof inside the ciphertext), one per round trip. The daemon sets
  `reputation_exported_at` in `users`.

The session is stateful across the two round trips, so both the issuer and the
client persist it; an abandoned session expires and does not consume the
once-only flag.

`reputation_exported_at` is stored **day-truncated**, with the same
`day_truncate` helper as `since` (section 6.1). It exists only to enforce the
once-only rule, and a second-precision value would hand a leaked source
database exactly the timestamp needed to correlate an export with a redemption
— the correlation section 3 tries to bound. Session state, which is
necessarily fine-grained, is deleted when the session completes or expires.

### 5.4 Redemption (import)

A new action `import-reputation` carrying the token, sent over the daemon's
ordinary protocol transport. The destination daemon:

1. Checks the issuer is in its trusted list and the epoch is accepted.
2. Verifies `s·G == R + H(R ‖ m)·P_cell` with the `P_cell` published for the
   token's cell and epoch. This needs only public data, which is the whole
   reason for blind Schnorr; the cell whose key verifies is the band.
3. Checks the pubkey embedded in `m` equals `UnwrappedMessage.identity` —
   the identity the transport *proved*, not one the sender claims. On
   protocol v2 that proof is `identity_sig`, a domain-tagged signature bound
   to the trade pubkey authoring the event, so it cannot be grafted from
   another sender. This binds the token to one identity; selling it means
   handing over the key, which is the same risk as selling the account today.
4. Checks the token id `H_"mostro/reputation/token/v1"(m)` (section 5.1) is
   not in `redeemed_reputation_tokens`, and that this `(issuer, identity)`
   pair has not been redeemed before.
5. Inserts the redemption and seeds the user row (section 6) in the same
   transaction.

Replies: `reputation-imported` on success; `cant-do` with one of the new
`CantDoReason` variants on failure (section 9, PR 2.3). `CantDoReason` is the
one enum in core carrying `#[serde(other)]`, so an old client degrades a new
reason to `Unknown` instead of failing. `Action` and `Payload` have no such
fallback — see section 9 for why that is still safe.

## 6. Merging on the destination

Each dimension has its own merge rule, because they measure different things:

- **Ratings received add up.** A rating on A and a rating on B are distinct
  reviews by distinct counterparties, so the seed is added to the local count.
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
The math lives next to `User::update_rating` in mostro-core, which already
owns the running-average formula (first vote weighted 1/2, then incremental
mean). A seeded user has `total_reviews > 1` from the start, so the
first-vote damping no longer applies to them; that is the intended anchor
effect.

| `users` column | Effect |
|---|---|
| `total_reviews` | `+= reviews_floor` |
| `total_rating` | recomputed as the weighted average of the existing average and the seed floor |
| `created_at` | `= min(created_at, now - age_floor)` — the displayed age |
| `native_created_at` (new) | **unchanged**; the account's own creation date, never backdated |
| `min_rating`, `max_rating`, `last_rating` | unchanged if non-zero, otherwise set to `round(rating_floor)` |
| `seeded_reviews` (new) | `+= reviews_floor`, internal only |
| `seeded_rating_sum` (new) | `+= reviews_floor * rating_floor`, internal only |

`native_created_at` is set once, when the row is created, and no import ever
moves it. Without it the age dimension would leak across hops: A→B backdates
B's `created_at`, and B exporting to C would then present A's age as its own
native age, so a seed would travel twice. Reviews and rating are protected the
same way by `seeded_reviews` and `seeded_rating_sum`; age needs its own field
because the merge is a minimum rather than a subtraction.

The public `rating` tag on order events keeps its three-field shape:
`{"total_reviews", "total_rating", "since"}` (see 6.1). No `legacy` marker is published.
The seeded review count acts as an anchor, so new ratings move the average
slowly, exactly as they would for a long-standing Mostro user.

Worked example. User with 10 days on Mostro and 2 reviews at 4.5 imports from
lnp2pBot where they have 347 completed trades, **214 ratings received**, 4.87
average and 3 years. The cell is `reviews:200+ | rating:4.5+ | age:24m+`, so
the seed is 200 reviews at 4.5 and 720 days; the 347 trades only decided
eligibility:

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
`since = min(since_local, since_seed)`.

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
  computes the cell from `total_reviews - seeded_reviews`, the native rating
  derived from `seeded_rating_sum`, and `native_created_at` rather than the
  backdated `created_at`. Seeds never travel twice, which
  rules out A→B→A doubling and A→B→C chains. A user who wants A and C on B
  redeems one token from each; the destination records each issuer once per
  identity.
- **One identity across instances.** A token binds the destination identity
  pubkey inside `m`, and redemption requires that key to sign, so a single
  token serves one identity — on as many instances as trust the issuer. The
  app already uses one identity key for every instance, so this is the normal
  case. Carrying reputation to a *fresh* identity per instance would need one
  token per identity, and nothing could then stop a user from redeeming two of
  them under two identities on the same instance: the destination cannot tell
  they are the same person, which is precisely the property the design
  provides. Fresh identity per instance and Sybil resistance are mutually
  exclusive here, and Sybil resistance wins.
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
  redeemable on any number of Mostros.** The binding is to the identity, not
  to the instance: the issuer never learns where the token is spent, and the
  same identity can redeem it on every instance that trusts the issuer.
  Since reputation is per-instance, redeeming on N instances grants exactly
  what the user had, once each. Restricting the number of destinations is
  unenforceable without a shared ledger anyway, and issuing per-destination
  tokens would tell the issuer which instances the user uses.
- **Sybil on one instance** (several identities sharing one reputation) is
  blocked by the identity binding plus the once-only issuance flag. This is
  what rules out fresh identity per instance (section 7).
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
`mostro` (daemon), `mobile` (app 1.x, pure Dart), `app` (app 2.x, Flutter UI
over a Rust core through flutter_rust_bridge, already on mostro-core, nostr-sdk
0.45 and `k256`), `lnp2pbot/bot`.

Both apps must support the migration. They differ in where the work lands:
the 1.x app needs its own Dart implementation of everything (blind Schnorr,
models, parsing), while the 2.x app gets the types from mostro-core and does
the curve arithmetic with `k256` inside its Rust core, adding only bridge
functions and UI on the Dart side.

**Protocol compatibility.** `PROTOCOL_VER` stays at 2 and the new actions do
not gate on it. In core 0.14.6 only `CantDoReason` carries `#[serde(other)]`;
`Action` and `Payload` would fail to deserialise an unknown variant. That is
safe here because these messages are strictly request/response over targeted
encrypted direct messages: `export-reputation` and `import-reputation` are only ever sent by
a client that implements them, and `reputation-exported` / `reputation-imported`
only ever come back to the client that asked. No new variant is broadcast, so
no old client is ever handed one. PR 2.3 carries a test that pins this
reasoning.

### Phase 0: specification

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 0.1 | protocol | Add `since` to the `rating` tag and to the kind 38384 event, day-truncated Unix timestamp. Mark `days` deprecated with a removal version. | Spec merged, example events updated. |
| 0.2 | protocol | Reputation band grid as a protocol constant: the three dimensions, cuts, floors, cell id string format (`reviews:200+\|rating:4.5+\|age:24m+`), the deterministic K-anonymity merge rule and its raw→effective map. | Open decisions 1-3 closed and written down. |
| 0.3 | protocol | Issuer keyset event: kind number, `d` tag, `epoch` tag, content schema, accepted-epochs rule. | Kind number reserved. |
| 0.4 | protocol | Actions `export-reputation`, `reputation-exported`, `import-reputation`, `reputation-imported` — those four kebab-case strings are the wire discriminators, used verbatim in the schema, the core enum serialization, the compatibility note and the vectors. Payload schemas; new `cant-do` reasons; the redemption checks in section 5.4 as normative text. | Spec merged; one name per action across every document. |
| 0.5 | protocol | Test vectors for the section 5.1 encoding (tagged hashes, point and scalar bytes, a canonical `m`, a cell id and a token id); keyset with a `merges` map; a full clause-blind-Schnorr transcript (`x_cell`, `k_0`, `k_1`, `α_i`, `β_i`, `R_i`, `c'_i`, `b`, `s'_b`, `s`) for fixed randomness; a valid token; and a table of invalid tokens (wrong cell key, wrong identity, expired epoch, mauled `s`, mauled `R`). | Vectors file committed; every implementation below tests against it. |

### Phase 1: `since` rollout

Independent of the migration and worth shipping first.

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 1.1 | mostro-core | `src/rating.rs`: `Rating` gains `since: Option<u64>` (serde default); `to_tags()` emits a `since` tag when set, `from_tags()` parses it; `Rating::new` keeps its signature and a `with_since(u64)` builder is added so no caller breaks. `src/user.rs`: `UserInfo` gains `since: Option<u64>` (serde default). Unit tests for both round trips. | Released as a **patch** (0.14.7): additive, no signature changes. |
| 1.2 | mostro | Bump core. One helper `day_truncate(created_at) -> u64` in `util.rs`. `create_rating_tag` (`nip33.rs`) emits `days` **and** `since`; `rate_user.rs` builds the `Rating` with `.with_since()` and keeps pushing the `days` tag; the `UserInfo` built in `util.rs` fills `since`. Unit tests on the three emitters. | All three sites carry both fields on a local relay. |
| 1.3 | mobile | `data/models/rating.dart` parses `since` and falls back to `days`; `data/models/user_info.dart` parses `since` and falls back to `operating_days`; age is computed at display time. Tests for both shapes. | Old and new daemons render the same age. |
| 1.3b | app | Rust core: `nostr/order_events.rs::parse_rating_tag` reads `since` and falls back to `days`; `mostro/status.rs` maps `UserInfo.since` with fallback to `operating_days`; bump mostro-core to 0.14.7. Bridge exposes `since` and the Dart side (`peer_reputation_card.dart`, trade detail) computes the age at display time. Tests for both shapes. | Old and new daemons render the same age. |
| 1.4 | mostro | Remove `days` and `operating_days` emission after the deprecation window (open decision 6). `UserInfo.operating_days` removal in core is a **minor** bump and goes with PR 2.3. | Removed with a changelog entry. |

### Phase 2: shared types in mostro-core

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 2.1 | mostro-core | Module `reputation::bands`: `ReviewsBand`, `RatingBand`, `AgeBand` enums with half-open cuts and floors, `Cell { reviews, rating, age }`, `Cell::from_stats(total_reviews, avg_rating, since, now)`, `Cell::id()` / `parse()`, and `apply_merges(&MergeMap)` implementing the transitive fold. Pure, exhaustively tested against 0.2 including every boundary value. | Round-trips every cell id; boundary values land in exactly one band; the fold terminates on a cyclic map instead of looping. |
| 2.2 | mostro-core | `ReputationKeyset` (parse/build the event from 0.3, epoch handling, `cells` **and** `merges`) and `ReputationToken { issuer, epoch, cell, m, r_point, s }` with serde and shape validation. Plus `reputation::encoding`: tagged hashes, point/scalar codecs, `m` builder and parser, token id — the section 5.1 contract, no signing. | Parses the 0.5 vectors byte for byte. |
| 2.3 | mostro-core | `src/message.rs`: `Action` variants `ExportReputation`, `ReputationExported`, `ImportReputation`, `ReputationImported` (past participle for daemon replies, matching `Released` / `Canceled`); `Payload` variants `BlindedReputationRequest(BlindedReputationRequest)`, `BlindedReputationResponse(BlindedReputationResponse)`, `ReputationToken(ReputationToken)`. `src/error.rs`: `CantDoReason` variants `UntrustedReputationIssuer`, `InvalidReputationToken`, `ReputationAlreadyRedeemed`, `ReputationIdentityMismatch`, `ExpiredReputationKeyset`, `NotEligibleForReputationExport`, `ReputationAlreadyExported`, inserted before `Unknown`. `src/prelude.rs`: `NOSTR_REPUTATION_KEYSET_KIND`. Serde round-trip tests, plus a test asserting an old client never receives these variants (`PROTOCOL_VER` stays 2, see the compatibility note above). | Part of the **minor** release. |
| 2.4 | mostro-core | `src/user.rs`: `User` gains `seeded_reviews: i64`, `seeded_rating_sum: f64`, `native_created_at: i64`, `reputation_exported_at: Option<i64>` (day-truncated), each with `#[sqlx(default)]` and `#[serde(default)]` so a daemon on an un-migrated database still deserialises `SELECT *`. `User::new` initialises `native_created_at` to `created_at`. | First use of `sqlx(default)` in core; test with a row that lacks the columns; existing rows backfill `native_created_at` from `created_at`. |
| 2.5 | mostro-core | `src/user.rs`: `User::apply_reputation_seed(&mut self, cell: &Cell, now: i64)` implementing section 6 next to `update_rating`, plus `User::native_stats(&self) -> (reviews, rating, since)` that subtracts the seeded part and reads `native_created_at`, never `created_at`. Property tests: never decreases `total_reviews`, never moves `created_at` forward, never touches `native_created_at`, and an A→B→C chain exports the same native stats B had before importing from A. | No I/O; released as **0.15.0** together with 2.1-2.4. |

### Phase 3: mostrod as destination (import)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 3.1 | mostro | Settings section `[reputation_import]` (`enabled`, `issuers`, `accepted_epochs`) with parsing, defaults and validation. No behaviour. | Bad config is rejected at startup with a clear error. |
| 3.2 | mostro | Bump core to 0.15.0. Migration `users` gains `seeded_reviews`, `seeded_rating_sum`, `native_created_at` (backfilled from `created_at`) and `reputation_exported_at` (matching PR 2.4 exactly, `SELECT *` + `FromRow` requires it); new table `redeemed_reputation_tokens(token_id PK, issuer, identity_pubkey, cell, redeemed_at)` with a unique index on `(issuer, identity_pubkey)`. `db.rs` accessors with tests. | Migration applies on an existing database; the `users` insert in `db.rs` binds the new columns. |
| 3.3 | mostro | Crypto module `reputation::verify` over `k256` (new dependency), using `reputation::encoding` from core so the byte contract is shared: challenge hash and the `s·G == R + H(R‖m)·P_cell` check. Tested against the 0.5 vectors, including every invalid case. | No daemon wiring yet. |
| 3.4 | mostro | Keyset fetcher: fetch and cache the issuer keyset event per trusted issuer, refresh on epoch change, reject unknown epochs. Tested with the `local-relay` feature. | Cache survives a relay outage. |
| 3.5 | mostro | Handler `src/app/import_reputation.rs`: routing in `app.rs`, checks 1-5 of section 5.3 in one transaction, calls `User::apply_reputation_seed` from core, persists through a new `update_user_reputation_seed` in `db.rs`, replies `reputation-imported` or `cant-do`. Integration test end to end with a fixture issuer. | A second redemption of the same token is rejected. |
| 3.6 | mostro | Publish the updated kind 38384 rating event after a successful import, reusing `update_user_rating_event`. | Event visible on the local relay. |

### Phase 4: mostrod as issuer (export)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 4.1 | mostro | Settings `[reputation_export]` (`enabled`, `epoch_length`); per-cell key derivation from the daemon key and epoch (HKDF, deterministic, never stored). Unit tests: same inputs, same keys. | No publication yet. |
| 4.2 | mostro | Publish the keyset event at startup and on epoch rollover: cell populations from the database, the deterministic merge from section 4, and both `cells` and the raw→effective `merges` map. | Event validates against 2.2; a client applying `merges` reproduces the issuer's assignment for every raw cell. |
| 4.3 | mostro | Native stats for the export cell come from `User::native_stats` in core (reviews and rating minus the seeded part, `native_created_at` for age). `count_completed_orders_for_identity` in `db.rs` over `orders.master_buyer_pubkey` / `master_seller_pubkey` with status `success` feeds eligibility only (full-privacy orders carry no master pubkey and are simply not counted). Tested. | Seeded values are excluded from the cell. |
| 4.4 | mostro | Handler `export_reputation_action`: the two-round-trip clause protocol (open session with `R'_0`/`R'_1`, then answer one clause), session store with expiry, eligibility, once-only flag, `reputation_exported_at`. Integration test: export from instance A, import on instance B, both in-process. | Round trip passes on two local daemons; an abandoned session expires without consuming the flag. |

### Phase 5: mobile (app 1.x, `mobile`)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 5.1 | mobile | Blind Schnorr client primitives in Dart over secp256k1: blinding factors, challenge, unblinding and signature verification. Tested against the 0.5 vectors. | No UI, no services. |
| 5.2 | mobile | Models: keyset event parser, `ReputationToken`, band cell; `MostroMessage` support for the new actions and payloads. | Serde round-trip tests. |
| 5.3 | mobile | Storage: Sembast repository for tokens and for in-flight session state (the open clauses, `α_i`, `β_i`, `m`, issuer) so an interrupted flow resumes. | Survives app restart in a test. |
| 5.4 | mobile | `MostroService` + notifier: `exportReputation(issuer)` and `importReputation(token)` flows against a Mostro issuer, with the random redemption delay. | Integration test against a local daemon from 4.4. |
| 5.5 | mobile | Telegram transport: the two deep-link round trips with the bot, cell validation against the user's own stats, signature verification, store token. | Manual test with the bot from phase 6. |
| 5.6 | mobile | Settings screen "Import reputation": issuer list from the selected node's trust list, status per issuer, localized strings in every `intl_*.arb`. | `flutter analyze` clean, gen-l10n reports no untranslated keys. |

### Phase 5b: app 2.x (`app`)

Runs in parallel with phase 5; shares the UI copy and the localized strings.

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 5b.1 | app | Rust core: bump mostro-core to 0.15.0 so `Cell`, `ReputationKeyset`, `ReputationToken`, the new `Action` / `Payload` / `CantDoReason` variants come from core. Add `rust/src/mostro/reputation.rs` with the blind Schnorr client side over `k256`, already a dependency. Tested against the 0.5 vectors. | No bridge, no UI. |
| 5b.2 | app | Rust core: token and in-flight blinding state persisted in the existing SQLite (native) / IndexedDB (web) layer, so an interrupted flow resumes on every platform. | Survives restart in a test on native and WASM. |
| 5b.3 | app | Rust core: `export_reputation(issuer)` and `import_reputation(token)` flows against a Mostro issuer through the existing message queue, with the random redemption delay; keyset fetch and cache per trusted issuer. Bridge functions exposed through flutter_rust_bridge (generated, never hand-written). | Integration test against the local daemon from 4.4. |
| 5b.4 | app | Telegram transport: the two round trips from Dart, through the deep link scheme on mobile and a paste field on desktop and web, handing each response to the Rust core for cell validation and signature verification. | Manual test with the bot from phase 6. |
| 5b.5 | app | Dart UI: settings screen "Import reputation" reusing the 1.x copy, localized in every ARB under `lib/l10n/`. | `flutter analyze` clean, no untranslated keys. |

### Phase 6: lnp2pBot as issuer

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 6.1 | bot | `REPUTATION_ISSUER_SK` in `.env-sample` and config validation — this is the bot's issuer identity: it signs the keyset, appears in trusted lists and fills the token `issuer` field, and is deliberately not `NOSTR_SK`. Per-cell key derivation from it (same HKDF scheme as 4.1); `@noble/curves` dependency for secp256k1 arithmetic. Unit tests on derivation. | No command yet; the derived pubkey matches what 6.2 publishes. |
| 6.2 | bot | Publish the keyset event through the existing `nostr` module at startup, signed with `REPUTATION_ISSUER_SK`, with the deterministic merge and its `merges` map computed from Mongo. | Event validates against 2.2. |
| 6.3 | bot | `User.reputation_exported_at` (day-truncated); `computeCell(user)` and `isEligible(user)` in `util/`, the cell from `total_reviews`, `total_rating` and `created_at`, eligibility from `trades_completed`, `disputes` and `banned`. Tests on band edges. | Pure functions only. |
| 6.4 | bot | `/start migrate_...` handler: the two round trips (open session, then answer one clause), session store with expiry, eligibility, once-only, reply with the app deep link; error messages in every locale YAML. Tests with a mocked user. | Round trip with 5.5. |

### Phase 7: rollout

1. Deploy 1.2 and 1.3 first; wait one release before 1.4.
2. Deploy phase 3 on the reference instance with an empty issuer list.
3. Deploy phase 6 on the bot and phase 4 on the reference instance; add both keys to the trust list.
4. Ship the 1.x app with phases 5.1-5.6 and the 2.x app with phases 5b.1-5b.5.
5. Document the operator side (`docs/REPUTATION_PORTABILITY.md`, settings template) and the user side (mobile in-app help).

## 10. Open decisions

1. Band cuts for the three dimensions (section 4).
2. `K` minimum cell population (default 50).
3. Eligibility minimums (default 10 completed trades, 1 rating, disputes lost < 10%).
4. Keyset event kind number and epoch length (default yearly).
5. Admin override for re-issuance: yes or no.
6. Length of the `days` deprecation window before PR 1.4 (default one minor release).
7. Width of the random redemption delay window (default 24h, section 3).

Closed during review:

- **Signature scheme.** Blind Schnorr on secp256k1, clause variant. BDHKE was
  ruled out because the destination cannot verify a BDHKE token without the
  issuer's private key.
- **`PROTOCOL_VER`.** Stays at 2; the new actions do not gate on it (section 9).
- **Trades vs reviews.** The first band dimension counts ratings received;
  completed trades decide eligibility only.
- **Fresh identity per instance.** Dropped, as incompatible with Sybil
  resistance (section 7).
- **Canonical encoding.** Fixed in section 5.1: compressed points, big-endian
  scalars, BIP-340-style tagged hashes, a fixed-length 70-byte `m`. Points are
  not x-only because even-Y normalisation would break the blinding.
- **K-anonymity vs client validation.** The merge is deterministic and its
  raw→effective map ships in the keyset, so the client folds its own cell the
  same way instead of aborting.
- **Native account age.** `native_created_at` is never backdated, so an
  imported age cannot be re-exported as native.
- **Issuer identity.** One key per issuer; for the bot that is
  `REPUTATION_ISSUER_SK`, not `NOSTR_SK`.
- **Action naming.** `reputation-exported` is the wire name for the export
  response, everywhere.
