# Reputation Portability

Design proposal for letting a user carry reputation earned in one trading venue
(lnp2pBot, or another Mostro instance) into a Mostro instance, as the
figures they earned there.

Status: **proposal**. Values marked *open decision* are defaults to be
confirmed before implementation.

## 1. Goals

1. A user with reputation on lnp2pBot can claim it on a Mostro instance.
2. The same mechanism lets a user copy reputation from one Mostro to another.
3. What travels is the **real** reputation — ratings received, average rating
   (rounded to two decimals), and the date of the first completed trade — not
   a band or bucket standing in for it.
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
- the destination operator learns the source account the attestation names
  (for a Mostro issuer, the user's identity pubkey there);
- anyone who knows the user's figures on the source can recognise them, merged,
  on the destination;
- with the lnp2pBot transport, Telegram sees the whole attestation: the
  destination identity, the source account id and the figures.

The figures are not secret on the source: a Mostro publishes a user's
`total_reviews` and `total_rating` in the `rating` tag of every order they
make. They are published under trade pubkeys, though, not under the identity,
and the triple of review count, average and first-trade day is close to
unique. The import is what joins that triple to an identity on a second
venue, and a visible jump in the user's rating events right after an import
says so too. A user who does not want the two identities linked simply does
not import.

Two consequences of the design are accepted rather than solved:

- **Figures are a snapshot.** An attestation reflects the source at issuance.
  A ban or a run of bad ratings on the source after that is not reflected on
  the destination, and an attestation stays valid until it expires
  (section 5.1).
- **Imported history dilutes local history.** A large imported record
  outweighs a few bad local ratings, exactly as a long native record would.
  The destination operator bounds this only by choosing whom to trust.

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
| Only the account holder can export | A Mostro issuer exports only for the identity the transport proved; lnp2pBot only for the Telegram account that asks (section 5.2) |
| An attestation serves one destination identity | It names that identity; redemption requires the transport's identity proof to match (section 5.3) |
| One source account is bound to one destination identity at a time | The issuer binds the source account to one destination identity, atomically, after the user confirms it (section 5.2). A rebind moves the binding but revokes nothing already issued, so across instances an account can end up seeding the old identity on some and the new one on others; on any one instance it seeds only one (next row) |
| A mistaken binding can be undone by its owner | Rebinding needs a signature from the currently bound identity, or an audited admin action (section 5.2) |
| A source account is counted once per destination | The destination records `(issuer, subject)` and refuses a second import (section 5.3) |
| A seed never travels twice | A Mostro issuer exports native reputation only, and never imports its own attestations (sections 5.3, 7) |
| Stale figures cannot be imported later | Attestations expire, and the destination caps their lifetime (section 5.3) |

## 4. What travels

Three figures, all of them the user's own native reputation on the source:

| Field | Meaning | Mostro issuer | lnp2pBot issuer |
|---|---|---|---|
| `reviews` | Ratings received | `total_reviews - seeded_reviews` | `total_reviews` |
| `rating` | Average of those ratings, rounded to two decimals | `native_rating_sum / native_reviews` | `total_rating` |
| `since` | Day-truncated date of the first completed trade | earliest `success` order with the identity as master buyer or seller | earliest completed order of the user |

- **Ratings received, not completed trades.** On Mostro `total_reviews` only
  increments when a counterparty actually submits a rating, and it is also the
  weight of the running average. Seeding it from a trade count would publish a
  review count the user never earned and give a single rating the weight of
  hundreds. Completed trades are not carried.
- **`since` is the first completed trade, not account creation.** An account
  that registered years ago and never traded has no trading history to carry;
  measuring from creation would let a dormant account import "three years
  trading". It is day-truncated with the same rule as the public `rating` tag
  (section 6.1), so the attestation carries no more precision than is already
  public. Order history is local to each venue, so this date is native by
  construction: an import never changes it.
- **Eligibility**: not banned, at least **10 completed trades** and at least
  **5 ratings received**, both counted on the source's native history only.
  Below that the issuer refuses. The floor makes a reputation expensive to
  fabricate with a handful of throwaway trades.

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
| `reviews` | Decimal integer, `≥ 5` (the eligibility floor) |
| `rating` | Decimal string with exactly two fractional digits, in `[1.00, 5.00]` |
| `since` | Unix seconds, a multiple of 86400, `≥ 1577836800` (2020-01-01, before any issuer existed) and `≤ created_at` |
| `expiration` | [NIP-40](https://github.com/nostr-protocol/nips/blob/master/40.md) Unix seconds; `created_at` + the attestation lifetime (*open decision*, default 7 days) |
| `z` | `reputation-attestation` |

Each tag appears exactly once. The issuer computes `rating` from its internal
average as `clamp(round(avg × 100), 100, 500) / 100`, in IEEE 754 double
precision, with `round` rounding half away from zero, and formats it with
exactly two decimals. Fixing the arithmetic makes Rust, JavaScript and Dart
agree even where the decimal value is not representable (`4.895 × 100` is
`489.49999999999994`, so it gives `4.89`, not `4.90`). The clamp matters for legacy Mostro rows,
whose damped average can sit below 1 (section 7). Two decimals is what every client displays, so the
rounding never shows; it is not bit-exact, and the merge in section 6 works
on the rounded value.

The kind (*open decision*, default `38388`) gives the signature its own
domain: an issuer key that also signs other events can never have one of
those accepted as an attestation. An issuer never signs any other event of
this kind, and the rebind authorisation of section 5.2, which reuses it, is
signed by a user identity that is never on a trust list.

The issuer key is **dedicated** to issuance on every issuer. For lnp2pBot it
is `REPUTATION_ISSUER_SK`, kept apart from the bot's `NOSTR_SK`; for a Mostro it
is a separate key in `[reputation_export]`, not the daemon key, so either can
be rotated without touching the other, and the daemon key never signs
attestations next to its ordinary public events.

### 5.2 Issuance (export)

One request and one reply; there is no session.

1. **The issuer authenticates the source account.** The request is only ever
   answered for the account that sends it:
   - a Mostro issuer requires `UnwrappedMessage.identity` (on protocol v2,
     proved by `identity_sig`) and sets `subject` to that proved identity. It
     never reads the source account from the payload, and it refuses a
     request without an identity proof — a `full_privacy` user or a bare
     trade key has no identity-bound reputation to export;
   - lnp2pBot takes the source account from the Telegram user who sent the
     command, never from the link.
2. The issuer looks up the user and checks eligibility (section 4).
3. **Confirmation, on a first binding.** If the account is not bound yet, the
   issuer shows the destination identity as an `npub` and asks the user to
   confirm it before anything is recorded. A link opened by mistake, or one
   crafted by someone else, binds nothing without that confirmation. On a
   Mostro issuer the request is itself signed by the source identity, so the
   app asks for the confirmation before sending it.
4. **Binding.** The confirmed destination identity is recorded in
   `reputation_exported_to` with an atomic compare-and-set: it succeeds only
   if the field is empty or already holds that identity (`UPDATE … WHERE
   reputation_exported_to IS NULL OR reputation_exported_to = ?` in the
   daemon, `findOneAndUpdate` with the same condition in the bot). Two
   concurrent requests for different identities therefore cannot both bind.
   A later export to the bound identity is how a user re-obtains a lost
   attestation or a fresher one; an export to any other identity is refused
   unless the request carries a rebind authorisation.
5. The issuer signs the attestation with its issuer key and returns it.
6. The client verifies the signature and shows the user the figures before
   importing.

**Rebinding.** A binding is undone only by its owner or by an operator:

- **Rebind authorisation.** The user signs, with the *currently bound*
  identity, an event of the attestation kind with the tags `["p", "<new
  identity>"]`, `["issuer", "<issuer pubkey>"]`, `["z", "reputation-rebind"]`
  and an `expiration` at most one hour after its `created_at`, and sends it
  with the export request. The issuer checks the signature, that `pubkey`
  equals `reputation_exported_to`, that `issuer` names itself and that it has
  not expired, then moves the binding to the new identity with the same
  compare-and-set (conditioned on the old value) and exports to it. The
  `issuer` tag keeps an authorisation from being replayed at another issuer;
  the compare-and-set makes a replay at the same one a no-op.
- **Admin rebind.** A user who lost the bound identity cannot sign, so the
  operator can rebind by hand. Every admin rebind is logged with the old and
  new identity and the reason.

Rebinding does not let one source account seed two identities on the same
instance: the destination refuses a second import of the same
`(issuer, subject)` regardless of the identity (section 5.3, step 7).

A rebind does not revoke anything already issued. Imports the old identity
made stay where they are, and an attestation issued to it stays redeemable
until it expires, at most the destination's lifetime cap. An issuer cannot
withdraw what it never sees redeemed, and destinations do not ask the issuer
about the current binding, so after a rebind the same source account can seed
the old identity on some instances and the new one on others. This is
accepted: reputation is per instance, and the per-instance check above is what
stops one account from backing several identities where it matters.

Transport differs per issuer:

- **lnp2pBot**: a Telegram deep link `t.me/lnp2pbot?start=rep_<pubkey>`,
  where `<pubkey>` is the destination identity as 43 characters of unpadded
  base64url — Telegram caps the start parameter at 64 characters, which a hex
  key with a prefix would exceed. The bot shows the `npub` with a confirm
  button (step 3), then replies with a button opening the app's deep link with
  the attestation; on desktop and web the user pastes it. A rebind
  authorisation does not fit in the start parameter, so the app shows it for
  the user to paste into the bot chat, which answers with the same confirm
  step.
- **Mostro**: an `export-reputation` message over the daemon's ordinary
  protocol transport (v2: NIP-44 `kind: 14`, authored by a trade key with the
  identity proof inside the ciphertext), with the destination identity and
  an optional rebind authorisation in the payload; the daemon replies
  `reputation-exported` with the attestation.

A node advertises what it supports in its kind 38385 info event, as it already
does for Serbero: `["reputation_import_issuers", "<hex>", …]` lists its trust
list (absent when import is disabled), and `["reputation_issuer", "<hex>"]`
names its issuer key (absent when export is disabled). Clients read these to
offer the import and export screens, and never send the new actions to a node
that does not advertise them.

### 5.3 Redemption (import)

A new action `import-reputation` carrying the attestation, sent over the
daemon's ordinary protocol transport. The destination daemon:

1. Parses the event and verifies its id and signature.
2. Checks the kind, the `z` tag, and every tag rule in section 5.1.
3. Checks `pubkey` is in its trusted issuer list, and is **not** its own
   issuer key. A Mostro trusting itself would otherwise let a user import
   their own reputation on the same instance and double it.
4. Checks the times, allowing a clock skew of 300 seconds: `created_at` is not
   in the future, `expiration` has not passed, and `expiration - created_at`
   does not exceed the destination's own maximum lifetime (default 7 days).
   The cap keeps a buggy or compromised issuer from minting attestations that
   live for years.
5. Requires `UnwrappedMessage.identity` and checks the `p` tag equals it — the
   identity the transport *proved*, not one the sender claims. On protocol v2
   that proof is `identity_sig`, a domain-tagged signature bound to the trade
   pubkey authoring the event, so it cannot be grafted from another sender. A
   message without an identity proof is refused; there is no fallback to the
   trade key.
6. Loads the user row keyed by that proved identity; the import is applied to
   that row and no other.
7. Checks that neither `(issuer, subject)` nor `(issuer, identity)` has been
   imported before on this instance. The first stops a source account from
   seeding two identities here, even after a rebind or if an issuer broke its
   own binding; the second stops an identity from importing two accounts from
   one issuer. Both are unique indexes, so two concurrent imports cannot both
   pass.
8. Records the import and applies it to the user row (section 6) in the same
   transaction.

Replies: `reputation-imported` on success; `cant-do` with one of the new
`CantDoReason` variants on failure (section 9, PR 2.2). `CantDoReason` is the
one enum in core carrying `#[serde(other)]`, so an old client degrades a new
reason to `Unknown` instead of failing. `Action` and `Payload` have no such
fallback — see section 9 for why that is still safe.

The issuer's identity is **one key**: it signs attestations, and it is what a
destination lists as trusted (section 5.1).

**Revoking an issuer.** Removing a key from the trust list stops new imports
but does not unwind earlier ones. Because every import is kept as a row of
`reputation_imports` with its figures (section 6), an operator who learns that
an issuer key was compromised can list the imports whose attestation was
signed after a given date and reverse each one. The row keeps the
attestation's `created_at` for this: the import date says nothing about when
the attestation was signed, and the attestation itself is on no relay. The
reversal: subtract its `reviews` from `total_reviews` and
`seeded_reviews`, take `rating × reviews` back out of the weighted average and
of `seeded_rating_sum`, and recompute `created_at` as the minimum of
`native_created_at` and the `since` of the imports that remain. Native ratings
received in between are unaffected, because the running average is linear in
each contribution. `min_rating`, `max_rating` and `last_rating` are reset to
`0` when no rating remains at all; otherwise they are left as they are. An
import sets them only on a row with no rating (section 6), so reverting it
right away restores them exactly, but once native ratings have arrived in
between, a revoked import's value can survive in `max_rating` or `min_rating`.
That is accepted: those fields are informational tags of the kind 38384 event,
no decision reads them, and recomputing them would need a per-review history
the daemon does not keep.

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
| `native_created_at` (new) | **unchanged**; the account's own creation date, which revoking an import restores `created_at` from (section 5.3) |
| `min_rating`, `max_rating`, `last_rating` | unchanged if non-zero, otherwise set to `round(rating)` |
| `seeded_reviews` (new) | `+= reviews`, internal only |
| `seeded_rating_sum` (new) | `+= rating × reviews`, internal only |
| `native_rating_sum` (new) | **unchanged**; incremented by `update_rating` on native reviews only, internal only |

Each import is also kept as a row of `reputation_imports` (issuer, subject,
identity, the three figures, the attestation id, the attestation's
`created_at` and the import date): it is what step 7 of section 5.3 checks, it
keeps every seed attributable, and it is what revoking an issuer selects by
signing time (section 5.3).

`native_created_at` is set once, when the row is created, and no import ever
moves it. Because the merge for age is a minimum, `created_at` alone cannot be
undone; `native_created_at` is what an operator recomputes it from when
reversing an import. It is the row's creation date on purpose, not the first
completed trade: the daemon already publishes its native `since` from
`created_at` (mostro#1016), and a reversal must give back exactly the date the
user showed before the import, not a different one. Export does not read either column: the exported `since`
comes from the instance's own order history (section 4), so an imported date
can never be re-exported. Reviews and rating are kept apart by
`seeded_reviews`, `seeded_rating_sum` and `native_rating_sum`.

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

The daemon publishes both `days` and `since` at all three sites, and drops
`days` only as the very last step of the rollout (section 9, phase 7). The
window is not counted in releases: an operator decides when a daemon updates,
but nobody decides when users update their apps, and a client that predates
`since` loses the age the moment `days` goes away. Keeping it costs a few bytes
per event. `Rating::from_tags` ignores unknown keys and `UserInfo` gains the
field as `Option` with a serde default, so old and new peers interoperate for
as long as both fields are published.

## 7. Mostro-to-Mostro specifics

- **Only native reputation is exportable.** When a Mostro acts as issuer it
  takes `reviews` and `rating` from `User::native_stats`, which undoes the
  import arithmetic of section 6:

  ```text
  native_reviews = total_reviews - seeded_reviews
  native_rating  = native_rating_sum / native_reviews
  ```

  where `native_rating_sum` is a new `users` column that `update_rating`
  increments by the raw rating on every native review, and that
  `apply_reputation_import` never touches. Reversing `total_rating` instead is
  not exact: the first-vote 1/2 weighting means `total_rating × total_reviews`
  is the sum of ratings minus half the first one. `since` and the completed
  trade count come from the instance's own `success` orders, where the
  identity is the master buyer or seller (`full_privacy` orders carry no
  master pubkey and do not count), so neither can include imported history.
  Imports never travel twice, which rules out A→B→A doubling and A→B→C
  chains. A user who wants A and C on B imports one attestation from each.
- **Legacy rows.** Rows that exist before the migration have no per-review
  history to replay, so `native_rating_sum` is backfilled as
  `total_rating × total_reviews`. Their exported average is therefore the
  displayed one, damped by the first vote: up to `r₁ / (2n)` below the true
  mean, which with the 5-rating floor is at most half a star and shrinks with
  every rating. This is accepted: a legacy user exports what counterparties
  already see. The issuer's clamp to `[1.00, 5.00]` (section 5.1) covers the
  case where the damped value falls below 1. Rows created after the migration
  are exact.
- **No self-import.** An instance never accepts an attestation signed by its
  own issuer key (section 5.3, step 3). The same identity importing from
  Mostro A into Mostro B is the normal case; importing from A into A is not.
- **One identity across instances.** An attestation names the destination
  identity pubkey, and redemption requires that key to sign, so a single
  attestation serves one identity — on as many instances as trust the issuer.
  The app already uses one identity key for every instance, so this is the
  normal case.
- **Reputation is not publicly queryable by identity.** Mostro's rating events
  are keyed by trade pubkey, so a destination cannot simply read the source's
  relays; an issuer attestation is required even between Mostros.
- **Trust list.** Each instance configures accepted issuers and publishes
  them in its info event (section 5.2). The reference instance ships with the
  lnp2pBot issuer pubkey enabled. An instance that exports configures its own
  dedicated issuer key, read from the environment like every other secret.

```toml
[reputation_import]
enabled = true
max_lifetime = "7d"
issuers = [
  "<lnp2pbot issuer pubkey>",
  "<another mostro's issuer pubkey>",
]

[reputation_export]
enabled = true
issuer_key_env = "MOSTRO_REPUTATION_ISSUER_SK"
lifetime = "7d"
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
  destination's `(issuer, subject)` check (section 5.3). The second holds even
  across a rebind: an account rebound from X to Y and imported by X on an
  instance cannot be imported there again by Y.
- **Re-issuance is free to the bound identity.** A lost attestation, or one
  that expired before it was used, is simply requested again. Moving the
  binding to a different identity needs the old identity's signature or an
  audited admin action (section 5.2).
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
reasoning. The other direction — a new client and an old daemon, which would
fail to deserialise the new actions and never answer — is closed by the info
event: a client sends them only to a node that advertises
`reputation_import_issuers` or `reputation_issuer` (section 5.2).

### Phase 0: specification

Phase 0 starts once open decisions 1-3 of section 10 are signed off: the kind
number, the attestation lifetime and the lnp2pBot `subject` are written into
the schema of 0.2 and the vectors of 0.4, and changing them afterwards means
redoing both. Decision 4 does not block it.

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 0.1 | protocol | Add `since` to the `rating` tag and to the kind 38384 event, day-truncated Unix timestamp. Mark `days` deprecated with a removal version. | Done (protocol#59). |
| 0.2 | protocol | Reputation attestation event: kind number reserved, tag schema and rules from section 5.1 (including the rounding and clamp of `rating`), the rebind authorisation event, the export authentication, confirmation and binding rules of section 5.2 and the redemption checks of section 5.3 as normative text. | Kind number reserved. |
| 0.3 | protocol | Actions `export-reputation`, `reputation-exported`, `import-reputation`, `reputation-imported` — those four kebab-case strings are the wire discriminators, used verbatim in the schema, the core enum serialization, the compatibility note and the vectors. Payload schemas, with the optional rebind authorisation in the export request; new `cant-do` reasons. The `reputation_import_issuers` and `reputation_issuer` tags of the kind 38385 info event. | Spec merged; one name per action across every document. |
| 0.4 | protocol | Test vectors: an issuer secret key, a valid attestation signed with it and its event id, the merge of section 6 on a fixed user row, the reversal of that merge, `rating` rounding and clamping cases (`4.8666… → 4.87`, `4.125 → 4.13`, `4.895 → 4.89`, `0.9 → 1.00`), a valid and an invalid rebind authorisation, and a table of invalid attestations (wrong kind, missing or repeated tag, `rating` out of range or not exactly two decimals, `reviews` below 5, `since` not day-aligned, before 2020 or after `created_at`, expired, lifetime over the cap, future `created_at` beyond the skew, `p` not matching the identity, mauled signature, untrusted issuer, own issuer key). | Vectors file committed; every implementation below tests against it. |

### Phase 1: `since` rollout

Independent of the migration and worth shipping first.

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 1.1 | mostro-core | `src/rating.rs`: `Rating` gains `since: Option<u64>` (serde default); `to_tags()` emits a `since` tag when set, `from_tags()` parses it; `Rating::new` keeps its signature and a `with_since(u64)` builder is added so no caller breaks. `src/user.rs`: `UserInfo` gains `since: Option<u64>` (serde default). Unit tests for both round trips. | Released (shipped in 0.15.0). |
| 1.2 | mostro | In progress (mostro#1016). Bump core. One helper `first_trade_since(created_at)` over core's `day_truncate`. `create_rating_tag` (`nip33.rs`) emits `days` **and** `since`; `rate_user.rs` builds the `Rating` with `.with_since()` and keeps pushing the `days` tag; the `UserInfo` built in `util.rs` fills `since`. Unit tests on the three emitters. | All three sites carry both fields on a local relay. |
| 1.3 | mobile | `data/models/rating.dart` parses `since` and falls back to `days`; `data/models/user_info.dart` parses `since` and falls back to `operating_days`; age is computed at display time. Tests for both shapes. | Old and new daemons render the same age. |
| 1.3b | app | Rust core: `nostr/order_events.rs::parse_rating_tag` reads `since` and falls back to `days`; `mostro/status.rs` maps `UserInfo.since` with fallback to `operating_days`. Bridge exposes `since` and the Dart side (`peer_reputation_card.dart`, trade detail) computes the age at display time. Tests for both shapes. | Old and new daemons render the same age. |
| 1.4 | mostro | **Deferred to the last step of the rollout (phase 7), not part of this phase.** Remove `days` and `operating_days` emission. `UserInfo.operating_days` is removed from core in the same step, as a **minor** bump, never earlier: a core that drops it makes every daemon built on it stop sending it. | Removed with a changelog entry. |

### Phase 2: shared types in mostro-core

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 2.1 | mostro-core | Module `reputation`: `ReputationAttestation { issuer, destination, subject, reviews, rating, since, created_at, expiration }`, `build(&Keys, …) -> Event` (rounding and clamping `rating`) and `parse(&Event, now, max_lifetime) -> Result<ReputationAttestation, _>` applying every rule of section 5.1 plus signature, kind, skew and lifetime checks; `ReputationRebind { issuer, new_identity, expiration }` with the same `build` / `parse` pair. The trust list, the self-issuer check and the identity match stay with the caller. Pure. | Parses and rejects the 0.4 vectors exactly. |
| 2.2 | mostro-core | `src/message.rs`: `Action` variants `ExportReputation`, `ReputationExported`, `ImportReputation`, `ReputationImported` (past participle for daemon replies, matching `Released` / `Canceled`); `Payload` variants `ReputationExportRequest { destination, rebind: Option<String> }` and `ReputationAttestation(String)` carrying the event JSON. `src/error.rs`: `CantDoReason` variants `UntrustedReputationIssuer`, `InvalidReputationAttestation`, `ExpiredReputationAttestation`, `ReputationIdentityMismatch`, `ReputationIdentityRequired`, `ReputationAlreadyImported`, `NotEligibleForReputationExport`, `ReputationBoundToOtherIdentity`, `InvalidReputationRebind`, inserted before `Unknown`. Serde round-trip tests, plus a test asserting an old client never receives these variants (`PROTOCOL_VER` stays 2, see the compatibility note above). | Part of the **minor** release. |
| 2.3 | mostro-core | `src/user.rs`: `User` gains `seeded_reviews: i64`, `seeded_rating_sum: f64`, `native_rating_sum: f64`, `native_created_at: Option<i64>`, `reputation_exported_to: Option<String>` and `reputation_exported_at: Option<i64>` (day-truncated), each with `#[sqlx(default)]` and `#[serde(default)]` so a daemon on an un-migrated database still deserialises `SELECT *`. `native_created_at` is an `Option` on purpose: `#[sqlx(default)]` on a plain `i64` would read a missing column as `0`, and reversing an import would then place every legacy user in 1970. `User::new` sets it to `Some(created_at)`; every reader goes through `User::native_created_at()`, which falls back to `created_at` when the column is absent. | Test with a row that lacks the columns and assert the fallback equals `created_at`, never `0`. |
| 2.4 | mostro-core | `src/user.rs`: `update_rating` gains `native_rating_sum += rating`, and nothing else changes in it. Own PR because it touches the hot path every rating goes through. Unit tests: one native review (rating 5 → `native_rating_sum` 5.0 even though `total_rating` is 2.5 by the first-vote rule); several native reviews sum to the plain total. | Existing `update_rating` tests unchanged and green. |
| 2.5 | mostro-core | `src/user.rs`: `User::apply_reputation_import(&mut self, &ReputationAttestation)` and `User::revert_reputation_import(&mut self, &ReputationImport, remaining_since: Option<i64>)` implementing section 6 and its reversal next to `update_rating`, plus `User::native_stats(&self) -> (reviews, rating)` implementing the section 7 formula. `apply_reputation_import` leaves `native_rating_sum` and `native_created_at` alone. Unit tests: the 0.4 merge and reversal vectors; several imports from different issuers; native reviews before and after an import. Property tests: never decreases `total_reviews`, never moves `created_at` forward, never touches `native_created_at` or `native_rating_sum`, apply-then-revert restores the row, and an A→B→C chain exports the same native stats B had before importing from A. | No I/O; released together with 2.1-2.4. |

### Phase 3: mostrod as destination (import)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 3.1 | mostro | Settings section `[reputation_import]` (`enabled`, `issuers`, `max_lifetime`) with parsing, defaults and validation; the `reputation_import_issuers` tag in the kind 38385 info event. No other behaviour. | Bad config is rejected at startup with a clear error; the tag is absent when import is disabled. |
| 3.2 | mostro | Bump core. Migration: `users` gains `seeded_reviews`, `seeded_rating_sum`, `native_rating_sum` (backfilled as `total_rating * total_reviews`, the legacy approximation of section 7), `native_created_at` (backfilled from `created_at`), `reputation_exported_to` and `reputation_exported_at`; new table `reputation_imports(attestation_id PK, issuer, subject, identity_pubkey, reviews, rating, since, signed_at, imported_at)`, `signed_at` being the attestation's `created_at`, with unique indexes on `(issuer, subject)` and `(issuer, identity_pubkey)`. `db.rs` accessors with tests; both the insert and every `users` update path persist `native_rating_sum`. | Migration applies on an existing database; a rating received after the migration moves `native_rating_sum`. |
| 3.3 | mostro | Handler `src/app/import_reputation.rs`: routing in `app.rs`, `ReputationAttestation::parse` from core, then the trust list, self-issuer, identity-required and uniqueness checks of section 5.3 and `User::apply_reputation_import` in one transaction; replies `reputation-imported` or `cant-do`. Integration test end-to-end with a fixture issuer key, including two concurrent imports of the same source. | A second import of the same source is rejected; an import without an identity proof is rejected. |
| 3.4 | mostro | Publish the updated kind 38384 rating event after a successful import, reusing `update_user_rating_event`. | Event visible on the local relay. |
| 3.5 | mostro | Admin RPC to revoke the imports of an issuer whose `signed_at` is after a given date, through `User::revert_reputation_import`, republishing the rating events. Logged. | Revoking an import with no native rating after it restores the row exactly in an integration test; an attestation signed before the cutoff but imported after it is left alone. |

### Phase 4: mostrod as issuer (export)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 4.1 | mostro | Settings `[reputation_export]` (`enabled`, `issuer_key_env`, `lifetime`) with the dedicated issuer key loaded from the environment, and the `reputation_issuer` tag in the info event. Eligibility from `User::native_stats`, `is_banned`, and a `db.rs` query over the identity's `success` orders returning the completed count and the first trade date. Tested. | Imported values never reach an attestation; the issuer key is never the daemon key. |
| 4.2 | mostro | Handler `export_reputation_action`: identity required, eligibility, the confirmation-gated binding of section 5.2 as one compare-and-set (`reputation_exported_to`, `reputation_exported_at`), sign with the issuer key, reply `reputation-exported`. Integration test: export from instance A, import on instance B, both in-process; two concurrent exports to different identities. | Round trip passes on two local daemons; exactly one of two concurrent bindings wins; an export to a second identity is refused. |
| 4.3 | mostro | Rebinding: the rebind authorisation in the export request (section 5.2), and an admin RPC to rebind by hand, logged with the reason. | A valid authorisation moves the binding once; a replayed one is a no-op; one signed by any other key is refused. |

### Phase 5: mobile (app 1.x, `mobile`)

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 5.1 | mobile | Models: attestation parsing and validation against the 0.4 vectors; `MostroMessage` support for the new actions and payloads. | Serde round-trip tests; every 0.4 vector accepted or rejected as specified. |
| 5.2 | mobile | `MostroService` + notifier: `exportReputation(issuer)` and `importReputation(attestation)` flows against a Mostro issuer, sent only to nodes whose info event advertises them; confirmation of the destination identity before a first export; signing a rebind authorisation with the old identity; the attestation is stored until imported. | Integration test against a local daemon from 4.2 and 4.3. |
| 5.3 | mobile | Telegram transport: the `start=rep_…` deep link, receiving the attestation through the app deep link, showing a rebind authorisation to paste into the bot, verification, and a confirmation screen showing the figures before import. | Manual test with the bot from phase 6. |
| 5.4 | mobile | Settings screen "Import reputation": issuer list from the selected node's info event, status per issuer, localized strings in every `intl_*.arb`. | `flutter analyze` clean, gen-l10n reports no untranslated keys. |

### Phase 5b: app 2.x (`app`)

Runs in parallel with phase 5; shares the UI copy and the localized strings.

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 5b.1 | app | Rust core: bump mostro-core so `ReputationAttestation` and the new `Action` / `Payload` / `CantDoReason` variants come from core; `export_reputation(issuer)`, `import_reputation(attestation)` and `sign_reputation_rebind(issuer, new_identity)` through the existing message queue, gated on the node's info event, the attestation persisted until imported. Bridge functions exposed through flutter_rust_bridge (generated, never hand-written). | Integration test against the local daemon from 4.2. |
| 5b.2 | app | Telegram transport: the deep link on mobile and a paste field on desktop and web, handing the attestation to the Rust core for validation. | Manual test with the bot from phase 6. |
| 5b.3 | app | Dart UI: settings screen "Import reputation" with the confirmation step, reusing the 1.x copy, localized in every ARB under `lib/l10n/`. | `flutter analyze` clean, no untranslated keys. |

### Phase 6: lnp2pBot as issuer

| PR | Repo | Scope | Done when |
|---|---|---|---|
| 6.1 | bot | `REPUTATION_ISSUER_SK` in `.env-sample` and config validation — the bot's issuer identity, deliberately not `NOSTR_SK`. `User.reputation_exported_to` and `reputation_exported_at` (day-truncated); `isEligible(user)` (not banned, 10 completed trades, 5 ratings) and `firstTradeSince(user)` over the user's completed orders in `util/`. Tests. | Pure functions and one query; a user whose order history cannot give a first trade date is not eligible. |
| 6.2 | bot | `/start rep_…` handler: decode the destination identity, eligibility, the `npub` confirm button, binding with `findOneAndUpdate` conditioned on the field being empty or equal, sign the attestation through the existing `nostr` module, reply with the app deep link; error messages in every locale YAML. Tests against the 0.4 vectors and with a mocked user, including two concurrent bindings. | Round trip with 5.3; exactly one of two concurrent bindings wins. |
| 6.3 | bot | Rebinding: accept a pasted rebind authorisation in the chat, verify it against the 0.4 vectors, confirm, and move the binding; an admin command for the lost-identity case, logged. | Rebind round trip with 5.3. |

### Phase 7: rollout

1. Deploy 1.2, then ship 1.3 and 1.3b.
2. Deploy phase 3 on the reference instance with an empty issuer list.
3. Deploy phase 6 on the bot and phase 4 on the reference instance; add both keys to the trust list.
4. Ship the 1.x app with phases 5.1-5.4 and the 2.x app with phases 5b.1-5b.3.
5. Document the operator side (`docs/REPUTATION_PORTABILITY.md`, settings template) and the user side (mobile in-app help).
6. Last of all, PR 1.4: stop publishing `days` and `operating_days`, and drop `UserInfo.operating_days` from core. Nothing earlier in the plan depends on it, and it never moves ahead of another step (section 6.1).

## 10. Open decisions

1. Attestation kind number (default `38388`, to be reserved in protocol PR 0.2).
2. Attestation lifetime (default 7 days, and the destination's cap on it).
3. lnp2pBot `subject`: internal user id or Telegram id (default internal id).
4. Refreshing an earlier import with newer figures: in v1 or later (default later).

Closed during review:

- **`days` deprecation window.** `days` and `operating_days` stay published
  until the last step of the rollout, not for a fixed number of releases:
  app updates are outside anyone's control (section 6.1).
- **Real figures, not bands.** Reputation travels as the user's real figures;
  unlinkability is not a goal, because importing is opt-in (section 3). This
  drops the band grid, K-anonymity, the per-cell keysets and blind Schnorr
  signatures.
- **`rating` as a two-decimal average**, rounded half away from zero and
  clamped into `[1.00, 5.00]`, rather than an integer sum of ratings. Two
  decimals is what clients display; the merge works on the rounded value.
- **`since` is the first completed trade**, not account creation, so a dormant
  account cannot import an age it never traded.
- **Eligibility**: not banned, at least 10 completed trades and 5 ratings
  received, on native history.
- **Export authentication.** A Mostro issuer exports only for the proved
  identity; lnp2pBot only for the Telegram user who asks.
- **Rebinding.** A first binding requires the user to confirm the destination
  `npub`. A binding moves only with a signature from the bound identity or an
  audited admin action.
- **Discovery.** Nodes advertise their trust list and issuer key in the info
  event; clients never send the new actions to a node that does not.
- **No self-import**, and a destination-side cap on attestation lifetime.
- **Dedicated issuer keys** on every issuer: a Mostro signs attestations with
  a key of its own, never with its daemon key.
- **Source identity in the attestation.** The `subject` tag names the source
  account, so the destination can refuse a second import of it.
- **Merged, not marked.** Imported reputation is merged into the ordinary
  fields and no marker is published.
- **`PROTOCOL_VER`.** Stays at 2; the new actions do not gate on it (section 9).
- **Trades vs reviews.** Ratings received are carried; completed trades decide
  eligibility only.
- **Native reputation only.** `seeded_reviews` and `native_rating_sum` keep an
  imported figure from being re-exported as native; `since` comes from local
  orders.
- **Issuer identity.** One dedicated key per issuer; for the bot that is
  `REPUTATION_ISSUER_SK`, not `NOSTR_SK`.
- **Action naming.** `reputation-exported` is the wire name for the export
  response, everywhere.
