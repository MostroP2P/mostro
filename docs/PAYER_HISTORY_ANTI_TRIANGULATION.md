# Payment Sender Verification & Payment-Account History — Anti-Triangulation Implementation Spec

**Status:** Draft for review · **Target:** `main` (`mostro-core 0.14.5` → needs a
`mostro-core` minor release, see §6) · **Feature flag:** `[payer_history].enabled`,
defaults to `false`

This document turns the
[anti-triangulation specification](https://gist.github.com/grunch/bf70cb2b3cadaa85d4374c40ab17b8d3)
into an engineering plan for `mostrod`, `mostro-core` and the protocol book. The
gist defines *what* the mechanism is and *why* it preserves Mostro's privacy
model; this document defines *how* it is built: the exact protocol surface, the
database schema, where each hook lands in the daemon, the authorization rules,
the client contract, the test plan and the PR breakdown.

Read the gist first. Its numbered sections are referenced here as `gist §N`.

---

## 1. Context — the problem we are attacking

### 1.1 The triangulation scam

Three participants:

- **Seller** — a legitimate Mostro user selling sats.
- **Attacker** — a malicious Mostro buyer.
- **Victim** — an unrelated third party, outside Mostro.

The attacker advertises something for sale outside Mostro (a phone, tickets,
furniture). The victim agrees to buy it. The attacker then takes a Mostro sell
order and, instead of paying the seller, gives the **seller's** fiat payment
details to the victim as "where to pay for the item". The victim pays the
seller. The seller sees the right amount at the right time, releases the sats
to the attacker, and is left holding a payment from somebody who never agreed
to buy bitcoin — and who will, sooner or later, dispute it with their bank or
report the seller for fraud.

### 1.2 Why the obvious fixes are not enough

| Mitigation | Why it fails (gist §5–§6) |
|---|---|
| A per-trade payment reference (`MOSTRO-8F21`) | The attacker simply tells the victim to use that reference. Possession of the reference proves nothing about *who* is paying. |
| Buyer declares the payer account, seller checks it matches | The attacker collects the victim's account details *before* taking the order and declares those. The incoming payment matches perfectly. |

Declared details + a matching sender therefore **cannot** prove that the
sender is the Mostro buyer. They only prove that the buyer knew, in advance,
which account the money would come from.

### 1.3 The asymmetry we can exploit

A legitimate buyer reuses one or a handful of fiat accounts across many
trades. A triangulation attacker must burn a *fresh* third-party account on
(almost) every trade — that is their business model. So the question Mostro
can usefully answer is not

> "Does this bank account belong to this person?"

but

> "Has **this Mostro user** already completed successful trades from **these
> same payment details**?"

A positive answer (many successful trades, over months, against many different
counterparties) is expensive to fake. A negative answer is exactly what a
triangulation attack looks like — and it is also what every honest first-time
user looks like, which is why this is a **risk signal, never an automatic
block** (gist §20, §33).

### 1.4 Two independent signals

The seller's client ends up showing two separate checks before release:

1. **Sender match** — does the fiat sender shown by the seller's bank match
   the details the buyer declared? (Client-side, human-verified.)
2. **Payment-account history** — how much successful history does *this
   buyer* have with *this payment identity*, and how experienced were the
   counterparties behind it? (Computed privately by `mostrod`, returned as
   four aggregate counters.)

A sophisticated attacker can satisfy (1). They cannot cheaply satisfy (2).

---

## 2. Goals and non-goals

### Goals
- Give the seller an aggregate, order-scoped history signal for the buyer's
  declared payment identity: `successful_trades`, `distinct_counterparties`,
  `experienced_counterparties`, `first_success_at`, `last_success_at`.
- Harden the counterparty signal against Sybil farming: separately count the
  counterparties that were already *experienced* on this node when they traded
  with the buyer, under node-configurable thresholds (D-7) — a flat one-hop
  signal, deliberately not recursive reputation.
- Let the seller's client verify that the payment details it received from the
  buyer are the same ones the buyer committed to Mostro (hash consistency).
- Build history **only** from trades that reach `Status::Success`.
- Never publish payer details, payment hashes or history on Nostr.
- Never expose a "same user?" oracle or any cross-trade linkage to
  counterparties (gist §12, §29).
- Keep `mostrod` byte-for-byte behaviour-preserving while the flag is off.
- Remain compatible with Full Privacy Mode (no crash, no leak, honest "no
  history available" signal — see §4 D-4).

### Non-goals
- Proving legal ownership of an account, KYC/AML, or any government identity.
- Deciding for the seller. Mostro provides information, not permission.
- Validating or canonicalising payment data **on the daemon**. Plaintext
  payer data never reaches `mostrod` (§4 D-2).
- A structured payment-method registry in `mostro-core`. `payment_method`
  stays the free-form string it is today; canonicalisation rules live in the
  protocol book (§7).
- Cross-instance portability of history, privacy-preserving proofs, scoring
  — future work (gist §37, §18 here).

---

## 3. Baseline — how `mostrod` works today (facts this design builds on)

Everything in this section is verified against `main` and `mostro-core 0.14.5`.
File references are to this repository unless marked `$CORE`
(`~/.cargo/registry/src/*/mostro-core-0.14.5/`).

### 3.1 Identity model

| Fact | Where |
|---|---|
| Every message carries a **trade key** (`event.sender`) and an **identity / master key** (`event.identity`). The master key is the only durable per-user handle the daemon has. | `$CORE/src/nip59.rs` (`UnwrappedMessage`), `handle_message_action_no_ln` in `src/app.rs` |
| `users.pubkey` (PRIMARY KEY) is the master key. A `users` row is created **only** when `identity != sender`. | `migrations/20231005195154_users.sql`, `handle_message_action_no_ln` in `src/app.rs` |
| `orders.master_buyer_pubkey` / `master_seller_pubkey` hold the identity keys; `buyer_pubkey` / `seller_pubkey` the trade keys. | `migrations/20221222153301_orders.sql`, `Order` buyer/seller pubkey helpers in `src/util.rs` |
| **Full Privacy Mode is structural, not a flag**: the client never sends the identity key, so `identity == sender` and `master_*_pubkey == *_pubkey`. No `users` row, no trade-index continuity, reputation reads as zero, rating a full-privacy counterpart is a silent no-op. | `Order` identity-key documentation in `$CORE/src/order.rs` (doc comment), `Order::is_full_privacy_order` usage in `src/util.rs`, `rate_user_action` in `src/app/rate_user.rs`, `Order::is_full_privacy_order` in `$CORE/src/order.rs` (`is_full_privacy_order`) |

**Consequence:** in Full Privacy Mode Mostro has *no* cross-trade continuity
for the buyer at all. The gist's "how Mostro identifies continuity between a
user's trade keys is an implementation detail" resolves, on this codebase, to:
*continuity = `master_buyer_pubkey`, which exists only in reputation mode.*

### 3.2 What Mostro relays between the parties

- The only counterparty data Mostro forwards today is a pubkey plus an optional
  reputation snapshot, via `Payload::Peer(Peer { pubkey, reputation })`
  (`fiat_sent_action` in `src/app/fiat_sent.rs`, `enqueue_order_msg` peer payload handling in `src/util.rs`).
- There is **no** buyer→seller payment-info channel through Mostro.
  `Action::SendDm` hits the `_` catch-all in `handle_message_action_no_ln` in `src/app.rs` and is ignored;
  peer chat is a pure client feature (`$CORE/src/chat/`, protocol `chat.md`).
- `orders.payment_method` is an unvalidated free-form string, public on the
  kind-38383 event as the multi-value `pm` tag (`info_to_tags` / order tag generation in `src/nip33.rs`).
- `Action::FiatSent` carries no payload except the optional `NextTrade`
  (`fiat_sent_action` in `src/app/fiat_sent.rs`).

### 3.3 Where "trade completed successfully" is known

There is **exactly one** writer of `Status::Success` for a normal trade:
`payment_success` in `src/app/release.rs`. It performs a CAS
`UPDATE orders SET status=?, event_id=? WHERE id=? AND status='settled-hold-invoice'`
and treats `rows_affected()==0` as "another task already finalised this order —
do not repeat side effects". It is reached from the in-process payout watcher
(`release` watcher in `src/app/release.rs`), from crash-recovery reconciliation (`payment_success` reconciliation in `src/app/release.rs`) and,
via `do_payment`, from admin settle (`admin_settle_action` in `src/app/admin_settle.rs`) and the
retry job (`scheduler` retry job in `src/scheduler.rs`).

The Cashu track defines a *different* success path — the release watcher in
`docs/cashu/03-track-b-release.md §5C` — which is not implemented yet. See §13.

### 3.4 What is public on Nostr

Kind 38383 (order) carries no pubkeys; kind 38384 (rating) is keyed by the rated
user's pubkey and carries only rating aggregates; kind 38385 (info) carries
node policy tags (e.g. `bond_policy_tags` from `src/nip33.rs`). Nothing about
fiat accounts exists on any event and this feature must keep it that way.

### 3.5 Existing patterns we reuse

| Pattern | Example |
|---|---|
| Party authorisation is open-coded per handler against `event.sender` | `fiat_sent_action` buyer check in `src/app/fiat_sent.rs` (`InvalidPubkey`), `release_action` seller check in `src/app/release.rs` (`InvalidPeer`) |
| Post-success, order-scoped, once-only authorisation | `rate_user_action` in `src/app/rate_user.rs` + `claim_order_rating_flag` CAS (`claim_order_rating_flag` in `src/db.rs`) |
| Opt-in feature section with `Option<…Settings>` and `#[serde(default)]` | `AntiAbuseBondSettings` (`AntiAbuseBondSettings` in `src/config/types.rs`), `Settings.anti_abuse_bond` (`Settings.anti_abuse_bond` in `src/config/settings.rs`) |
| Feature-owned module with its own `db.rs` | `src/app/bond/{mod,db,model,flow,…}.rs` |
| Migration with a long `--` design header and per-column comments | `migrations/20260423120000_anti_abuse_bond.sql` |
| Node policy advertised on the info event | `bond_policy_tags` (`bond_policy_tags` / info tags in `src/nip33.rs`) |
| Handler tests with in-memory sqlite + `TestContextBuilder` + `queued_actions_for(order_id)` | `fiat_sent_action` tests in `src/app/fiat_sent.rs` |

---

## 4. Locked design decisions (do not re-litigate per PR)

**D-1 · History subject = `orders.master_buyer_pubkey`.** It is the only
durable per-user handle the daemon has (§3.1). No new identity mechanism is
introduced.

**D-2 · Plaintext payer details never touch `mostrod`.** The buyer sends the
plaintext to the seller over the existing peer channel (protocol `chat.md`, or a
NIP-44 DM to the seller's trade key). Only the 32-byte `payment_hash` is sent
to Mostro. Mostro cannot leak, log or be subpoenaed for what it never receives.
This is stricter than gist §26 ("whenever possible store only the hash") and
costs nothing because the client already has a direct channel to the
counterparty.

**D-3 · Hash consistency is enforced by the seller's client, assisted by
Mostro.** Mostro forwards the buyer-committed `payment_hash` to the seller. The
seller's client recomputes the hash from the plaintext it received off-band and
flags a mismatch locally. Mostro never compares plaintext to anything.

**D-4 · Full Privacy Mode buyers get an honest "no history" answer, and nothing
is stored for them.** When `order.is_full_privacy_order()` reports the buyer
side as full-privacy, Mostro returns `buyer_mode: "full_privacy"` with zero
counters and **does not** write history rows (they could never be matched
again and would only link a hash to a trade key in the DB). Clients must render
this as *"history is unavailable for this buyer"*, not as *"new account"*.
Hash-only (user-agnostic) history is explicitly deferred to §18.

**D-5 · History increments in exactly one place: the `Success` CAS in
`payment_success`.** The increment runs only when `rows_affected()==1`, inside
the same function, and is idempotent by construction (the declaration row is
consumed atomically, §10.4). Any future success path (Cashu watcher, §13) must
call the same helper.

**D-6 · Disputed trades do not build history.** If `order.buyer_dispute` or
`order.seller_dispute` is set when the order reaches `Success`, the
declaration is discarded without incrementing. Rationale: a trade that needed a
solver is not clean evidence of a healthy buyer↔account relationship, and this
removes the cheapest "farm history while disputing" path. Revisit with data
(§17).

**D-7 · Counterparties are stored as a keyed hash, not as pubkeys, and carry
a monotone "experienced" qualification snapshot.**
`counterparty_id = sha256("mostro-payer-history-cp-v1" ‖ node_secret ‖ master_seller_pubkey)`
where `node_secret` is the node's Nostr secret key bytes. The history tables
alone cannot be joined back to `users`/`orders` without the node key, and a
full-privacy seller (whose master key is a fresh trade key) simply counts as a
new counterparty each time — which is the conservative direction.

**Experienced counterparty.** A distinct counterparty counts as *experienced*
for a `(buyer, payment_hash)` pair when, **at the moment a trade with that
pair reaches `Success`**, the seller already had — counting only history that
existed **before** that trade:

- at least `experienced_min_trades` (`N`) successful, undisputed Mostro
  trades; and
- at least `experienced_min_days` (`D`) operating days since its first such
  trade.

Qualification is computed from the `orders` table (§10.1) — the seller's whole
node-side record, including trades that predate this feature — not from the
history tables, so sellers never need the feature enabled to qualify. The
trade being recorded **never** counts toward its own counterparty's
qualification, and — an explicit, documented anti-Sybil choice — trades the
seller previously made **with the same buyer identity** do not count either:
without that exclusion a two-key Sybil (a seller bot that only ever trades
with the attacker's own buyer) qualifies at zero external cost; with it, the
fake seller must build real history with third parties. Because qualification
is therefore buyer-relative, the flag is stored on the
`(user_pubkey, payment_hash, counterparty_id)` row (§9), which is buyer-scoped
by construction. Full-privacy sellers never qualify: their master key is a
fresh trade key, so their qualifying history is always zero — the same
conservative direction as above.

The flag is **monotone** and is evaluated only when a success is recorded. A
counterparty stored with `experienced = 0` is upgraded to `1` only when a
*subsequent* successful trade with the same `(buyer, payment_hash)` pair
happens after the seller has crossed the thresholds; it is never flipped
retroactively merely because the seller's record improved later, and it never
flips back.

`N` and `D` are **node policy, not protocol constants**: operators set them in
`[payer_history]` (§8.1, defaults 5 and 30) and they are advertised on the
info event (§8.3) so clients can explain the metric without hard-coding
thresholds.

This is deliberately a flat, one-hop signal. It is **not** recursive
reputation: a counterparty's own `experienced_counterparties` value plays no
part in its qualification, and no graph scoring, PageRank or Web-of-Trust
weighting is applied.

**D-8 · Push, then pull.** Mostro *pushes* the history to the seller right
after `fiat-sent-ok`. A seller→Mostro *query* action exists for session
restore and clients that missed the push; it returns the same object and is
subject to the same authorization (§11). The push makes the MVP useful with
zero new client-initiated traffic; the pull is cheap because it reuses the same
code path.

**D-9 · Additive protocol surface only.** New `Action` variants, new `Payload`
variants, new `CantDoReason` variants. `SmallOrder` is **not** touched — it is
`#[serde(deny_unknown_fields)]` and positional (`SmallOrder` in `$CORE/src/order.rs`),
so any new field would break every existing client. `FiatSentOk` keeps its
`Payload::Peer` unchanged.

**D-10 · Off by default, inert when off.** With `[payer_history]` absent or
`enabled = false`: the new actions answer `cant-do invalid_action`, `fiat-sent`
is untouched, no table is written, and no payer-history info tags are emitted.

**D-11 · Mostro never blocks on history.** The only enforcement knob is
`require_declaration` (default `false`), which rejects `fiat-sent` when no
declaration exists. Even then the seller always keeps the final decision
(gist §20).

**D-12 · Domain-separated hash.** `payment_hash = sha256("mostro-payer-v1|" ‖ canonical_payment_data)`.
The gist specifies a bare `sha256(canonical)`; the fixed prefix costs nothing
and guarantees the value is useless outside this protocol (no accidental reuse
as an identifier elsewhere). Clients must agree on this exact prefix (§7).

---

## 5. Trade flow with the feature

Sell-order flow (buyer is the taker; the maker-buyer flow is symmetric — what
matters is only *which side is the buyer*):

```text
 Buyer (B)                      mostrod (M)                        Seller (S)
   |                               |                                  |
   |  take-sell / add-invoice ...  |   ... hold invoice paid ...      |
   |<----------- Active ---------->|<----------- Active ------------->|
   |                               |                                  |
   |  [client] canonicalise payer  |                                  |
   |  details, h = payment_hash    |                                  |
   |                               |                                  |
   |  declare-payer {h, pm} ------>|  validate, upsert declaration    |
   |<-- payer-declared {h, pm} ----|--- payer-declared {h, pm} ------>|  (S learns h)
   |                               |                                  |
   |  ===== plaintext payer details, peer channel (chat / DM) ======>|  (M never sees it)
   |                               |                                  |
   |  fiat-sent ------------------>|  (optional: require declaration) |
   |<-- fiat-sent-ok {peer} -------|--- fiat-sent-ok {peer} --------->|
   |                               |--- payment-history {stats} ----->|  PUSH (D-8)
   |                               |                                  |
   |                               |<-- payment-history (query) ------|  PULL, optional
   |                               |--- payment-history {stats} ----->|
   |                               |                                  |
   |                               |   S checks: sender == declared?  |
   |                               |   S reads history tiers          |
   |                               |<-- release  (or dispute) --------|
   |                               |                                  |
   |<-- purchase-completed --------|  Success CAS ok ──► record_payer_success (D-5)
```

### 5.1 Status matrix

| Action | Who | Allowed order status | Effect |
|---|---|---|---|
| `declare-payer` | buyer trade key | `WaitingPayment`, `WaitingBuyerInvoice`, `Active` | upsert declaration (last write wins) |
| `declare-payer` | buyer | `FiatSent` or later | `cant-do not_allowed_by_status` (frozen) |
| `payment-history` (push) | Mostro → seller | emitted inside `fiat_sent_action` after `fiat-sent-ok` | — |
| `payment-history` (query) | seller trade key | `FiatSent`, `Dispute`, `SettledHoldInvoice` | same object as push before success |
| `payment-history` (query) | seller trade key | `Success` | `cant-do not_found` after the declaration is consumed |
| success hook | daemon | `SettledHoldInvoice → Success` CAS | history += 1, counterparty qualification snapshot (D-7), declaration consumed |
| prune | scheduler | order in a terminal non-success status | declaration deleted |

`Active` is the normal declaration window. The two `Waiting*` statuses are
allowed so a client can collect the payer details during the same screen where
it collects the invoice, before the hold invoice is paid; nothing is
irreversible at that point.

---

## 6. Protocol changes (`mostro-core`)

These require a `mostro-core` **minor** release (`0.15.0` or `0.14.6` per the
project's versioning habit); `mostrod` PRs then bump the dependency. All
additions follow the existing serde conventions: `Action` is `kebab-case`,
`Payload` and `CantDoReason` are `snake_case`
(`Action`, `Payload` and `CantDoReason` in `$CORE/src/message.rs` / `$CORE/src/error.rs`).

### 6.1 `Action` (`$CORE/src/message.rs`)

```rust
/// Buyer → Mostro: commit to the payer identity (hash only) for this order.
DeclarePayer,
/// Mostro → buyer (ack) and Mostro → seller (forward): the committed hash.
PayerDeclared,
/// Seller → Mostro: request the aggregate history for this order.
/// Mostro → seller: the aggregate history (push after fiat-sent, or reply).
PaymentHistory,
```

Wire names: `declare-payer`, `payer-declared`, `payment-history`. Append at the
end of the enum (there is no wasm ABI on `Action`, but keeping declaration
order stable is house style).

### 6.2 `Payload` (`$CORE/src/message.rs`)

```rust
/// Buyer's commitment to the fiat payer identity for one order.
/// Carries only the hash; the plaintext goes buyer → seller off-band.
PayerDeclaration(PayerDeclaration),
/// Aggregate, order-scoped history of (buyer, payment_hash).
PaymentHistory(PaymentHistory),
```

```rust
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct PayerDeclaration {
    /// sha256("mostro-payer-v1|" || canonical_payment_data), 64 lowercase hex.
    pub payment_hash: String,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct PaymentHistory {
    /// Echo of the buyer-committed hash, so the seller's client can compare
    /// it with the hash it computes from the plaintext it received.
    pub payment_hash: String,
    /// "reputation" — counters are meaningful.
    /// "full_privacy" — counters are always zero; history is *unavailable*.
    pub buyer_mode: BuyerMode,
    /// Number of orders that reached `success` for (buyer, payment_hash).
    pub successful_trades: u32,
    /// Number of distinct sellers among those orders (keyed hash, see D-7).
    pub distinct_counterparties: u32,
    /// How many of those distinct counterparties were already *experienced*
    /// (D-7) at the time of their successful trade with this
    /// (buyer, payment_hash) pair. Thresholds are node policy, advertised
    /// on the info event (§8.3).
    pub experienced_counterparties: u32,
    /// Unix seconds of the first / last successful trade; `None` when
    /// `successful_trades == 0`.
    pub first_success_at: Option<i64>,
    pub last_success_at: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BuyerMode { Reputation, FullPrivacy }
```

Wire keys: `payer_declaration`, `payment_history`.

### 6.3 `CantDoReason` (`$CORE/src/error.rs`)

```rust
/// `payment_hash` is not 64 lowercase hex chars.
InvalidPaymentHash,
/// `fiat-sent` refused because the node requires a prior `declare-payer`.
PayerNotDeclared,
```

Wire names: `invalid_payment_hash`, `payer_not_declared`. Existing reasons are
reused for everything else: `invalid_action` (feature off), `not_found` (order),
`invalid_pubkey` (not the buyer), `invalid_peer` (not the seller),
`not_allowed_by_status`, `invalid_payload`.

### 6.4 `MessageKind::verify()` matrix (`MessageKind::verify()` in `$CORE/src/message.rs`)

The match is exhaustive on `Action`; add:

| Action | Requires |
|---|---|
| `DeclarePayer` | `id.is_some()` and `payload == Some(Payload::PayerDeclaration(_))` |
| `PayerDeclared` | `id.is_some()`; payload `PayerDeclaration` (Mostro-authored, lenient) |
| `PaymentHistory` | `id.is_some()`; payload `None` (query) **or** `PaymentHistory(_)` (reply/push) |

### 6.5 Wire examples (v2 transport, decrypted content tuple)

Buyer → Mostro:

```json
[
  {"order": {"version": 2, "request_id": 981231, "trade_index": 7,
             "id": "4f1c…", "action": "declare-payer",
             "payload": {"payer_declaration": {
               "payment_hash": "9b0e…c1"}}}},
  "<trade-key signature>",
  ["<identity pubkey>", "<identity proof signature>"]
]
```

Mostro → buyer ack:

```json
[
  {"order": {"version": 2, "request_id": 981231, "trade_index": null,
             "id": "4f1c…", "action": "payer-declared",
             "payload": {"payer_declaration": {
               "payment_hash": "9b0e…c1"}}}},
  null, null
]
```

Mostro → seller forward (unsolicited, so `request_id` is null):

```json
[
  {"order": {"version": 2, "request_id": null, "trade_index": null,
             "id": "4f1c…", "action": "payer-declared",
             "payload": {"payer_declaration": {
               "payment_hash": "9b0e…c1"}}}},
  null, null
]
```

Mostro → seller, pushed immediately after `fiat-sent-ok`:

```json
[
  {"order": {"version": 2, "request_id": null, "trade_index": null,
             "id": "4f1c…", "action": "payment-history",
             "payload": {"payment_history": {
               "payment_hash": "9b0e…c1",
               "buyer_mode": "reputation",
               "successful_trades": 47,
               "distinct_counterparties": 29,
               "experienced_counterparties": 11,
               "first_success_at": 1762128000,
               "last_success_at": 1787654321}}}},
  null, null
]
```

Seller → Mostro query: same wrapper, `"action": "payment-history"`,
`"payload": null`, signed by the seller's trade key. Reply as above with the
`request_id` echoed.

Full-privacy buyer:

```json
{"payment_history": {"payment_hash": "9b0e…c1", "buyer_mode": "full_privacy",
  "successful_trades": 0, "distinct_counterparties": 0,
  "experienced_counterparties": 0,
  "first_success_at": null, "last_success_at": null}}
```

No declaration at all (feature on, buyer never declared): the push is **not**
sent; a query answers `cant-do not_found`. Clients treat "no `payment-history`
message by release time" as its own (bad) signal.

---

## 7. Canonicalisation and `payment_hash` (client contract)

The daemon never sees plaintext (D-2), so canonicalisation is a **client
contract** that must be identical across clients or the same account yields
different hashes and history silently fragments. It is therefore specified in
the protocol book (new chapter `payer_declaration.md`, §15), not in code here.
Summary of the contract:

1. **Canonical string** = fields joined by `|`, in the fixed order defined per
   method, each field normalised as:
   - Unicode NFKC, then uppercase;
   - strip all whitespace, hyphens, dots and slashes from *identifier* fields
     (IBAN, CBU/CVU, PIX key, account number, tax id);
   - collapse runs of whitespace to one space in *name* fields, trim;
   - country codes ISO-3166 alpha-2, currency ISO-4217.
2. **Method prefix** = `<COUNTRY>|<METHOD>` (e.g. `AR|CVU`, `EU|SEPA`,
   `BR|PIX`), so identical account numbers under different rails never
   collide.
3. **Hash** = `sha256("mostro-payer-v1|" + canonical)` (D-12), hex, lowercase.
4. The hash MUST NOT include order id, trade key, timestamps or salt
   (gist §9) — those would make it unique per trade and defeat history.

Examples (canonical → hashed):

```text
AR|CVU|0000003100012345678901|27123456789
EU|SEPA|DE89370400440532013000|ALICE SMITH
BR|PIX|+5511999998888
```

`DE89 3704 0044 0532 0130 00` and `DE89370400440532013000` canonicalise to the
same string.

**Low-entropy warning.** A bank account + name is guessable by anyone who
already knows the account; the hash is not a secret, it is an *identifier that
must never be published*. This is exactly why D-2/D-10 keep it off Nostr and
why the node DB is the only place it lives.

Methods that cannot expose a sender (cash, gift cards, vouchers) have no
canonical form; clients MUST NOT declare a payer for them and SHOULD tell the
seller that sender verification is unavailable (gist §34).

---

## 8. Daemon — configuration and node policy advertisement

### 8.1 `settings.tpl.toml`

```toml
# Payment-account history / anti-triangulation (docs/PAYER_HISTORY_ANTI_TRIANGULATION.md).
# Opt-in, disabled by default. Mostro only ever stores a SHA-256 of the
# buyer's canonicalised payer details, keyed by the buyer's identity key.
#
# [payer_history]
# enabled = false
# # When true, `fiat-sent` is rejected with `payer_not_declared` unless the
# # buyer sent `declare-payer` first. Leave false unless your market relies
# # on sender-verifiable rails only.
# require_declaration = false
# # "Experienced counterparty" thresholds (D-7): a seller qualifies for a
# # buyer's history when, at the moment a trade with that buyer succeeds, it
# # already had >= experienced_min_trades successful, undisputed trades with
# # OTHER buyers ...
# experienced_min_trades = 5
# # ... and its first such trade was >= experienced_min_days days ago.
# experienced_min_days = 30
```

### 8.2 `src/config/types.rs`

```rust
/// Payment-account history (anti-triangulation). Opt-in; when `enabled`
/// is false every code path added by this feature is inert.
/// See `docs/PAYER_HISTORY_ANTI_TRIANGULATION.md`.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct PayerHistorySettings {
    #[serde(default)]
    pub enabled: bool,
    /// Reject `fiat-sent` when no `declare-payer` was received for the order.
    #[serde(default)]
    pub require_declaration: bool,
    /// "Experienced counterparty" thresholds (D-7). Node policy, advertised
    /// on the info event; NOT part of the protocol.
    #[serde(default = "default_experienced_min_trades")]
    pub experienced_min_trades: u32,
    #[serde(default = "default_experienced_min_days")]
    pub experienced_min_days: u32,
}

const fn default_experienced_min_trades() -> u32 { 5 }
const fn default_experienced_min_days() -> u32 { 30 }
```

`Settings` gains `#[serde(default)] pub payer_history: Option<PayerHistorySettings>`
(`src/config/settings.rs`, next to `anti_abuse_bond`) and three helpers mirroring
`is_cashu_enabled()`:

```rust
pub fn is_payer_history_enabled() -> bool
pub fn payer_declaration_required() -> bool   // implies enabled
pub fn payer_history_experience_thresholds() -> (u32, u32)   // (min_trades, min_days), defaults 5/30
```

### 8.3 Info event (kind 38385)

`info_to_tags` in `src/nip33.rs` appends `payer_history_tags(settings)`:

| Tag | Value |
|---|---|
| `payer_history_enabled` | `"true"` (only when enabled) |
| `payer_declaration_required` | `"true"` / `"false"` (only when enabled) |
| `payer_history_experienced_min_trades` | decimal string, e.g. `"5"` (only when enabled) |
| `payer_history_experienced_min_days` | decimal string, e.g. `"30"` (only when enabled) |

When the feature is disabled or absent, `info_to_tags` emits no payer-history tags, preserving disabled-mode output byte-for-byte. Clients treat missing tags as disabled, and read the threshold tags to explain the `experienced_counterparties` metric (D-7) without hard-coding node policy.

---

## 9. Daemon — data model

One migration, `migrations/2026MMDD120000_payer_history.sql`, in the house
style (long `--` header explaining *why*, per-column comments). Three tables;
**no** change to `orders` or `users`.

```sql
-- Per-order commitment. Short-lived: consumed on success, pruned on any other
-- terminal status. Holds the ONLY copy of a hash that is not yet history.
CREATE TABLE IF NOT EXISTS order_payer_declarations (
  order_id        char(36)  PRIMARY KEY NOT NULL,  -- orders.id (uuid)
  payment_hash    char(64)  NOT NULL,              -- sha256 hex, lowercase
  declared_at     integer   NOT NULL               -- unix secs of last upsert
);

-- Aggregate history. One row per (buyer identity key, payment hash).
-- Written ONLY from the Success CAS in release::payment_success.
CREATE TABLE IF NOT EXISTS payer_history (
  user_pubkey        char(64) NOT NULL,  -- orders.master_buyer_pubkey (reputation mode only)
  payment_hash       char(64) NOT NULL,
  first_success_at   integer  NOT NULL,
  last_success_at    integer  NOT NULL,
  successful_trades  integer  NOT NULL DEFAULT 0,
  PRIMARY KEY (user_pubkey, payment_hash)
);

-- Distinct-counterparty set. counterparty_id is a keyed hash (D-7), never a pubkey.
-- `experienced` is the D-7 qualification snapshot taken at success time; it may
-- flip 0→1 on a LATER success with the same triple, never retroactively.
CREATE TABLE IF NOT EXISTS payer_history_counterparties (
  user_pubkey        char(64) NOT NULL,
  payment_hash       char(64) NOT NULL,
  counterparty_id    char(64) NOT NULL,
  first_success_at   integer  NOT NULL,
  experienced        integer  NOT NULL DEFAULT 0, -- 1 = counterparty qualified (D-7); monotone 0→1
  PRIMARY KEY (user_pubkey, payment_hash, counterparty_id)
);
```

Notes
- `distinct_counterparties` is `COUNT(*)` on the third table and
  `experienced_counterparties` is `COUNT(*) WHERE experienced = 1`; not
  denormalised (the counterparty write is an upsert keyed by the triple, so
  both counts are exact).
- No foreign keys: `orders` rows outlive declarations, and the bond tables set
  the precedent of not declaring FKs.
- Sizes are trivial (two 64-char keys per successful trade).
- **Retention:** declarations are ephemeral (§10.5). History rows are kept
  indefinitely in the MVP; an admin RPC to purge a `user_pubkey` is listed in
  §18.
- The existing migration reconciler (`run_migrations` in `src/db.rs`) only special-cases
  `ADD COLUMN`; plain `CREATE TABLE IF NOT EXISTS` needs nothing extra.

---

## 10. Daemon — handlers and hooks

New module `src/app/payer/` (`mod.rs`, `db.rs`, `declare.rs`, `history.rs`,
`success.rs`), following `src/app/bond/`. All functions return
`Result<_, MostroError>` and map sqlx errors to
`MostroInternalErr(ServiceError::DbAccessError(..))`.

### 10.1 Shared helpers (`src/app/payer/db.rs`)

```rust
pub struct PayerDeclarationRow { pub order_id: Uuid, pub payment_hash: String,
                                 pub declared_at: i64 }

pub async fn upsert_declaration(pool, order_id: Uuid, hash: &str, now: i64)
    -> Result<(), MostroError>;                         // INSERT … ON CONFLICT(order_id) DO UPDATE
pub async fn find_declaration(pool, order_id: Uuid)
    -> Result<Option<PayerDeclarationRow>, MostroError>;
pub async fn take_declaration<'e, E: sqlx::Executor<'e>>(exec: E, order_id: Uuid)
    -> Result<Option<PayerDeclarationRow>, MostroError>; // DELETE … RETURNING *  (idempotency token)

pub struct HistoryCounters { pub successful_trades: u32,
                             pub distinct_counterparties: u32,
                             pub experienced_counterparties: u32,  // COUNT(*) WHERE experienced = 1
                             pub first_success_at: Option<i64>,
                             pub last_success_at: Option<i64> }

pub async fn load_history(pool, user_pubkey: &str, hash: &str)
    -> Result<HistoryCounters, MostroError>;
pub async fn bump_history<'e, E>(exec: E, user_pubkey, hash, counterparty_id,
                                 experienced: bool, now)
    -> Result<(), MostroError>;   // upsert payer_history + counterparty upsert (SQL below)

/// Counterparty qualification input (D-7). Reads `orders`, NOT the history
/// tables: the seller's whole node-side record counts, including trades that
/// predate this feature. Excludes `current_order` (already `Success` inside
/// the CAS transaction — the trade being recorded never counts) and every
/// trade with `buyer_pubkey` (only trades with OTHER buyers qualify — the
/// documented anti-Sybil choice, D-7).
pub struct SellerExperience { pub qualifying_trades: u32,
                              pub first_qualifying_at: Option<i64> }
pub async fn seller_experience<'e, E: sqlx::Executor<'e>>(exec: E,
    seller_master_pubkey: &str, buyer_pubkey: &str, current_order: Uuid)
    -> Result<SellerExperience, MostroError>;
pub async fn prune_declarations_for_terminal_orders(pool) -> Result<u64, MostroError>;
```

`bump_history`'s counterparty write — the qualification flag is monotone
(D-7): it can only ever go 0→1, on a later success, never back:

```sql
INSERT INTO payer_history_counterparties
  (user_pubkey, payment_hash, counterparty_id, first_success_at, experienced)
VALUES (?1, ?2, ?3, ?4, ?5)
ON CONFLICT(user_pubkey, payment_hash, counterparty_id)
DO UPDATE SET experienced = MAX(experienced, excluded.experienced);
```

`seller_experience`:

```sql
SELECT COUNT(*) AS n, MIN(created_at) AS first_at
  FROM orders
 WHERE master_seller_pubkey = ?1
   AND status = 'success'
   AND buyer_dispute = 0 AND seller_dispute = 0   -- "successful" means undisputed, as in D-6
   AND id <> ?2                        -- the trade being recorded never counts (D-7)
   AND master_buyer_pubkey <> ?3       -- trades with THIS buyer do not qualify (D-7)
```

`created_at` (order creation) is the only per-order timestamp available; it
overstates a trade's age by the trade's own duration, which is acceptable for
a days-granularity threshold and keeps the query to a single table.

Validation helper, used by the handler *and* by `bump_history`:

```rust
pub fn validate_payment_hash(h: &str) -> Result<(), MostroError> {
    let ok = h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if ok { Ok(()) } else { Err(MostroCantDo(CantDoReason::InvalidPaymentHash)) }
}
```

### 10.2 `declare_payer_action` (`src/app/payer/declare.rs`)

Wired in `handle_message_action_no_ln` (`handle_message_action_no_ln` in `src/app.rs`) as
`Action::DeclarePayer => declare_payer_action(ctx, msg, event, my_keys)`.

```rust
pub async fn declare_payer_action(ctx: &AppContext, msg: Message,
    event: &UnwrappedMessage, my_keys: &Keys) -> Result<(), MostroError> {
    if !Settings::is_payer_history_enabled() {
        return Err(MostroCantDo(CantDoReason::InvalidAction));            // D-10
    }
    let order = get_order(&msg, ctx.pool()).await?;                       // not_found
    if order.get_buyer_pubkey().ok() != Some(event.sender) {              // same check as `fiat_sent_action` buyer check in `src/app/fiat_sent.rs`
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }
    let status = order.get_order_status()?;
    if !matches!(status, Status::WaitingPayment | Status::WaitingBuyerInvoice | Status::Active) {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));      // frozen after fiat-sent
    }
    let decl = match msg.get_inner_message_kind().get_payload() {
        Some(Payload::PayerDeclaration(d)) => d.clone(),
        _ => return Err(MostroCantDo(CantDoReason::InvalidPayload)),
    };
    validate_payment_hash(&decl.payment_hash)?;
    db::upsert_declaration(ctx.pool(), order.id, &decl.payment_hash,
                           Timestamp::now().as_u64() as i64).await?;

    let request_id = msg.get_inner_message_kind().request_id;
    let payload = Some(Payload::PayerDeclaration(decl));
    enqueue_order_msg(request_id, Some(order.id), Action::PayerDeclared,
                      payload.clone(), event.sender, None).await;            // ack to buyer
    if let Ok(seller) = order.get_seller_pubkey() {                          // forward hash to seller
        enqueue_order_msg(None, Some(order.id), Action::PayerDeclared,
                          payload, seller, None).await;
    }
    Ok(())
}
```

Design notes
- Re-declaration overwrites. The seller receives every version; the client
  keeps the last one. There is no partial-update path.
- The seller may not exist yet in `WaitingBuyerInvoice` for a maker-buyer
  order whose taker has not paid — `get_seller_pubkey()` failing is not an
  error; the seller will get the hash with the `payment-history` push later.
- No spam-gate change: the sender is already in the known-keys lane as a
  party to a non-terminal order (`is_known_pubkey` in `src/db.rs`).

### 10.3 Push from `fiat_sent_action` and the optional gate

In `src/app/fiat_sent.rs`, after the `Active` status check and the buyer check and **before** the status transition:

```rust
if Settings::payer_declaration_required()
    && payer::db::find_declaration(pool, order.id).await?.is_none() {
    return Err(MostroCantDo(CantDoReason::PayerNotDeclared));
}
```

After `order_updated.update(pool).await?` succeeds and before returning:

```rust
if Settings::is_payer_history_enabled() {
    match payer::history::build_for_order(pool, ctx.keys(), &order_updated).await {
        Ok(Some(h)) => {
            enqueue_order_msg(None, Some(order_updated.id), Action::PaymentHistory,
                              Some(Payload::PaymentHistory(h)), seller_pubkey, None).await;
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("payer history push skipped for order {}: {e}",
                                  order_updated.id),
    }
}
```

`build_for_order` returns `None` when there is no declaration (no push, see
§6.5). The push is only queued after the local `FiatSent` transition is durable;
a history lookup failure after that point is logged and **must not** fail
`fiat_sent_action`.

### 10.4 `payment_history_action` — the seller query (`src/app/payer/history.rs`)

Wired as `Action::PaymentHistory => payment_history_action(ctx, msg, event, my_keys)`.

```rust
pub async fn payment_history_action(ctx, msg, event, _my_keys) -> Result<(), MostroError> {
    if !Settings::is_payer_history_enabled() { return Err(MostroCantDo(CantDoReason::InvalidAction)); }
    let order = get_order(&msg, ctx.pool()).await?;
    if order.get_seller_pubkey().ok() != Some(event.sender) {              // seller only, §11
        return Err(MostroCantDo(CantDoReason::InvalidPeer));
    }
    match order.get_order_status()? {
        Status::FiatSent | Status::Dispute | Status::SettledHoldInvoice => {}
        Status::Success => return Err(MostroCantDo(CantDoReason::NotFound)),
        _ => return Err(MostroCantDo(CantDoReason::NotAllowedByStatus)),
    }
    let history = build_for_order(ctx.pool(), ctx.keys(), &order).await?
        .ok_or(MostroCantDo(CantDoReason::NotFound))?;                     // buyer never declared
    enqueue_order_msg(msg.get_inner_message_kind().request_id, Some(order.id),
        Action::PaymentHistory, Some(Payload::PaymentHistory(history)), event.sender, None).await;
    Ok(())
}

pub async fn build_for_order(pool, node_keys: &Keys, order: &Order)
    -> Result<Option<PaymentHistory>, MostroError> {
    let Some(decl) = db::find_declaration(pool, order.id).await? else { return Ok(None) };
    let (normal_buyer_idkey, _) = order.is_full_privacy_order()?;
    let Some(user) = normal_buyer_idkey else {
        return Ok(Some(PaymentHistory::unavailable(decl.payment_hash)));   // D-4: all counters zero
    };
    let h = db::load_history(pool, &user, &decl.payment_hash).await?;
    Ok(Some(PaymentHistory { payment_hash: decl.payment_hash, buyer_mode: BuyerMode::Reputation,
        successful_trades: h.successful_trades,
        distinct_counterparties: h.distinct_counterparties,
        experienced_counterparties: h.experienced_counterparties,
        first_success_at: h.first_success_at, last_success_at: h.last_success_at }))
}
```

A seller restoring a session might re-query in `Status::Success`, but the
declaration row is consumed on success (§10.5), so the handler answers
`not_found` — the seller already received the push at `fiat-sent` time and
the value would now include the trade just completed, which is a different
(and confusing) number. Keep it simple: *post-success queries return
`not_found`.* (Listed in §17 as an open question if client authors want
otherwise.)

### 10.5 Success hook (`src/app/payer/success.rs`) — **D-5**

Called from `payment_success` in `src/app/release.rs` **only** on the
`rows_affected() == 1` branch, inside the Success transaction before commit and
before the `PurchaseCompleted` enqueue:

```rust
// release.rs, inside the same transaction that performs the Success CAS:
if Settings::is_payer_history_enabled() {
    payer::success::record_payer_success(&mut tx, ctx.keys(), &order_updated).await?;
}
```

```rust
pub async fn record_payer_success(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    node_keys: &Keys,
    order: &Order,
) -> Result<(), MostroError> {
    // Idempotency token: the declaration row can be consumed exactly once.
    let Some(decl) = db::take_declaration(&mut **tx, order.id).await? else {
        return Ok(());                                           // nothing to do / already done
    };
    if order.buyer_dispute || order.seller_dispute {                   // D-6
        return Ok(());                                           // declaration discarded
    }
    let (buyer_idkey, _) = order.is_full_privacy_order()?;
    let Some(user) = buyer_idkey else {
        return Ok(());                                           // D-4: nothing stored
    };
    let seller_master = order.get_master_seller_pubkey()
        .map(|k| k.to_string()).unwrap_or_else(|_| order.seller_pubkey.clone().unwrap_or_default());
    let cp = counterparty_id(node_keys, &seller_master);               // D-7
    // D-7 qualification snapshot. The CAS has already flipped this order to
    // Success *inside this transaction*, so the current trade must be excluded
    // explicitly: it never counts toward its own counterparty's qualification.
    // Only history that predates this trade — and only trades with OTHER
    // buyers — qualifies.
    let exp = db::seller_experience(&mut **tx, &seller_master, &user, order.id).await?;
    let (min_trades, min_days) = Settings::payer_history_experience_thresholds();
    let experienced = exp.qualifying_trades >= min_trades
        && exp.first_qualifying_at
               .is_some_and(|t| now() - t >= i64::from(min_days) * 86_400);
    db::bump_history(&mut **tx, &user, &decl.payment_hash, &cp, experienced, now()).await?;
    Ok(())
}

pub fn counterparty_id(node_keys: &Keys, seller_master_pubkey: &str) -> String {
    use bitcoin::hashes::{sha256, Hash, HashEngine};   // already a dependency (`src/util.rs`)
    let mut eng = sha256::Hash::engine();
    eng.input(b"mostro-payer-history-cp-v1");
    eng.input(node_keys.secret_key().as_secret_bytes());
    eng.input(seller_master_pubkey.as_bytes());
    sha256::Hash::from_engine(eng).to_string()          // lowercase hex
}
```

Why this is safe
- `payment_success` may be reached twice (watcher + reconciler, §3.3). The
  CAS guarantees only one caller enters the `rows_affected()==1` branch, and
  `take_declaration` (DELETE … RETURNING) guarantees at most one increment
  even if that invariant were ever broken. Because the history write shares the
  CAS transaction, a database failure rolls back the success transition and the
  declaration remains retryable by the existing payment-success retry paths.
- It does not touch `orders`, respecting the "no full-row writes after
  success" rule documented at `payment_success` in `src/app/release.rs` —
  `seller_experience` only *reads* `orders`, inside the same transaction.
- The `experienced` flag is monotone (D-7): the counterparty upsert keeps
  `experienced = MAX(experienced, excluded.experienced)` (§10.1), so a later
  success can upgrade 0→1 but nothing downgrades 1→0, and rows from past
  trades are never re-evaluated unless a new success lands — no retroactive
  upgrades.
- `CompletedByAdmin` / admin settle reach `Success` through the same
  `do_payment → payment_success` path and are covered; the dispute flags
  (D-6) decide whether they count.

### 10.6 Pruning (`src/scheduler.rs`)

A mode-agnostic job, `job_prune_payer_declarations`, every
`expiration`-class interval (reuse the 60 s cadence of
`job_process_dev_fee_payment`), running:

```sql
DELETE FROM order_payer_declarations
 WHERE order_id IN (SELECT id FROM orders WHERE status IN (<TERMINAL_ORDER_STATUSES>))
```

`TERMINAL_ORDER_STATUSES` already exists at `TERMINAL_ORDER_STATUSES` in `src/db.rs`. Success rows are
consumed in §10.5 before this job can see them, so the job only ever removes
declarations from cancelled / expired / admin-cancelled orders. Range-order
children are separate orders with separate declarations; nothing special.

### 10.7 Cashu dispatch

`dispatch_cashu` in `src/app.rs` currently rejects every action outside its
allow-list. `DeclarePayer` and `PaymentHistory` are **not** added there in the
MVP (the Cashu release path does not exist yet); see §13.

---

## 11. Authorization and privacy analysis

### 11.1 Who can learn what

| Party | Learns | Never learns |
|---|---|---|
| Seller (via Mostro) | buyer's committed `payment_hash` for **this order**; aggregate counters for **this** (buyer, hash) pair — including how many of the buyer's past counterparties met the node's experience policy (D-7), as a bare count; whether the buyer is in full-privacy mode (already visible today through `Peer.reputation == None`) | buyer identity key, other trade keys, other order ids, which sellers were the counterparties, any single counterparty's qualification status, any hash other than the one the buyer chose to commit to this order |
| Buyer | nothing new about the seller | — |
| Mostro node | `(master_buyer_pubkey, payment_hash)` association + counters; keyed counterparty hashes; per-counterparty qualification snapshots (derived from `orders`, which the node already holds) | plaintext payer details (D-2) |
| Public relays | nothing | everything in this feature |

### 11.2 Why there is no oracle

- The only query is `payment-history` and it takes **no parameters** beyond
  the order id. The `(user, hash)` pair is resolved server-side from the
  order. A seller cannot ask about a hash the buyer did not commit to this
  order, nor about a user who is not their counterparty in this order.
- Repeating the query returns the same numbers; it leaks nothing further.
- The seller already possesses the plaintext (the buyer sent it); learning
  its hash is not new information.
- The seller cannot distinguish "buyer B used account X before" from "some
  user used account X before" — they only see counters for the buyer they are
  *currently* trading with, which is exactly gist §29.
- There is no `same_user(a, b)` primitive and none is introduced (gist §12).
- Counterparty identities are keyed hashes (D-7); even an exported DB does not
  list which sellers a buyer dealt with without the node secret.
- The experienced count is computed from `orders` — data the node already
  holds — and is returned only as an aggregate, never per counterparty. The
  query takes no parameters, so a seller cannot turn it into an "is seller X
  experienced?" probe about a third party; they only ever learn how many of
  *this* buyer's past counterparties qualified.

### 11.3 Residual risks (accepted for the MVP)

| Risk | Mitigation / status |
|---|---|
| Node DB compromise reveals `(identity key, payment_hash)` pairs. Hashes are brute-forceable by anyone who already knows the candidate account. | Same trust boundary as `master_*_pubkey` and `buyer_invoice` today. History is inherently a node-side function; a node that wants less retention can purge (§18). |
| Buyer colludes with sellers to farm history. | Needs real successful trades with real sats, distinct counterparties and elapsed time (gist §32); tiers in §12 weight all four. Qualifying as an *experienced* counterparty additionally requires N prior successes with **other** buyers over D days (D-7), so a closed two-party Sybil ring no longer counts. |
| Victim happens to be a Mostro user with history on the same account. | Only a problem under hash-only history (§18); D-1 keys on the *buyer's* identity, so the victim's history is not attributed to the attacker. |
| Buyer declares a hash but sends different plaintext to the seller. | Seller's client recomputes and flags mismatch (D-3); treated like a sender mismatch — recommend dispute. |
| Buyer in full-privacy mode gets no history forever. | Deliberate (D-4). Sellers who care can prefer reputation-mode buyers; clients must word it as "unavailable", not "new". |

---

## 12. Client contract and UX guidance

Normative for clients that opt in (checked via the info-event tags, §8.3).

**Buyer side**
1. When the order is taken and `payer_history_enabled` is true, show a
   "payment sender" form for the method in use, explaining why (gist §21).
2. Canonicalise, hash (§7), send `declare-payer`. Keep the plaintext locally.
3. Send the plaintext to the seller over the peer channel (chat or DM).
4. Before `fiat-sent`, confirm: *"Did you send the payment from the account
   declared for this trade?"*
5. If `fiat-sent` answers `payer_not_declared`, go back to step 1.

**Seller side**
1. On `payer-declared`, store the hash for the order. On receiving the
   plaintext from the buyer, recompute; if it differs, show a hard warning.
2. On `payment-history` (push or reply), render two independent blocks:
   *Sender match* (manual confirmation) and *Payment-account history*.
3. In the history block, render `experienced_counterparties` alongside the raw
   counters, e.g. *"N of the buyer's past counterparties were already
   experienced on this node when they traded with them"*. Read the thresholds
   from the info-event tags (§8.3) — they are node policy (D-7), do not
   hard-code them.
4. Never auto-release; never auto-refuse. The release screen shows both blocks
   above `[Release]` / `[Dispute]` (gist §22).
5. If no `payment-history` arrived by the time fiat is reported sent and the
   node has the feature enabled, show *"Buyer did not declare a payment
   sender"* as its own warning.

**Suggested tiers** (client policy, not protocol):

| Tier | Condition (all of) | Wording |
|---|---|---|
| 🟢 Established | `successful_trades ≥ 5`, `distinct_counterparties ≥ 3`, `experienced_counterparties ≥ 1`, `first_success_at` ≥ 30 days ago | "Established payment account" |
| 🟡 Limited | `successful_trades ≥ 1` and not Established | "Limited payment history" |
| 🔴 New | `successful_trades == 0` (`buyer_mode == reputation`) | "No previous successful trades with this account" |
| ⚪ Unavailable | `buyer_mode == full_privacy` | "History unavailable (buyer trades in full-privacy mode)" |

Requiring ≥ 1 experienced counterparty keeps a fresh Sybil cluster from
reaching *Established* by trading only with itself: at least one counterparty
must already have had real history with third parties at trade time (D-7).

Forbidden wording: "verified owner", "verified account", "trusted account"
(gist §33).

---

## 13. Cashu mode — cross-track obligation

The Cashu release track (`docs/cashu/03-track-b-release.md §5C`) makes a
scheduler watcher the **only** path to `Success`. When that lands it MUST call
`payer::success::record_payer_success` on its own `SettledHoldInvoice → Success`
CAS success branch, and `dispatch_cashu` MUST add `DeclarePayer | PaymentHistory`
to its routing once `FiatSent` is routed there. Until then the feature is
Lightning-only; `Settings` validation SHOULD warn (not fail) at boot when both
`[cashu].enabled` and `[payer_history].enabled` are set.

---

## 14. Test plan

All tests are in-file `#[cfg(test)]` modules using the existing scaffolding
(`create_test_pool`, `TestContextBuilder`, `queued_actions_for(order_id)` —
`fiat_sent_action` test scaffolding in `src/app/fiat_sent.rs`). Coverage target for the new module: ≥ 80 %.

**`mostro-core`**
- Round-trip serde for the three actions / two payloads / two reasons; exact
  wire strings (`declare-payer`, `payer_declaration`, `invalid_payment_hash`);
  `PaymentHistory` round-trips with `experienced_counterparties` (older
  clients simply ignore the unknown key — the struct has no
  `deny_unknown_fields`).
- `MessageKind::verify()` accepts / rejects the matrix in §6.4.

**`declare_payer_action`**
- feature off → `invalid_action`; unknown order → `not_found`.
- seller sends it → `invalid_pubkey`; non-party → `invalid_pubkey`.
- each status in §5.1 (allowed vs `not_allowed_by_status`).
- bad hash (wrong length, uppercase, non-hex) → `invalid_payment_hash`;
  wrong payload → `invalid_payload`.
- happy path: row upserted, `PayerDeclared` queued to buyer (with request_id)
  and seller (without); re-declaration overwrites and re-notifies.
- maker-buyer order with no seller yet: only the buyer ack is queued.

**`fiat_sent_action` integration**
- feature off: byte-identical behaviour (existing tests unchanged).
- feature on, no declaration, `require_declaration=false`: no push, order
  moves to `FiatSent`.
- `require_declaration=true`, no declaration → `payer_not_declared`, status
  unchanged.
- with declaration: `FiatSentOk`×2 **and** one `PaymentHistory` to the seller
  with zero counters; with seeded history rows the counters are returned.
- full-privacy buyer: push has `buyer_mode = full_privacy`, zero counters.

**`payment_history_action`**
- buyer queries → `invalid_peer`; wrong statuses → `not_allowed_by_status`;
  no declaration → `not_found`; happy path echoes `request_id`.

**`record_payer_success`**
- increments `successful_trades`, sets first/last, inserts counterparty;
  second call is a no-op (row consumed).
- same buyer+hash, two different sellers → `distinct_counterparties == 2`;
  same seller twice → `1`.
- `buyer_dispute = 1` → declaration consumed, no history row.
- full-privacy buyer → declaration consumed, no history row.
- `counterparty_id` is deterministic for a key and differs across keys.
- seller below the thresholds → counterparty row stored with `experienced = 0`
  and `experienced_counterparties` reads 0; a seller with ≥ N qualifying
  trades whose first qualifying trade is ≥ D days old → `experienced = 1`.
- the trade being recorded never counts: a seller whose N-th qualifying trade
  is the current one does NOT qualify (D-7).
- buyer-relative exclusion: a seller whose prior successes were all with this
  same buyer identity does NOT qualify, however many there were (D-7).
- disputed past trades count neither toward N nor toward D.
- upgrade path: a first success stores `experienced = 0`; once the seller has
  crossed the thresholds with other buyers, a later success with the same
  (buyer, hash, counterparty) triple flips the stored flag to 1, and the
  `MAX()` upsert never flips it back.
- no retroactivity: the seller crossing the thresholds *after* a trade leaves
  previously stored rows at `experienced = 0` until another success lands.

**`payment_success` wiring** (extend `src/app/release.rs` tests with the
`PayoutStatusLookup` stub pattern, `payment_success` tests in `src/app/release.rs`)
- history recorded in the same transaction as the CAS-success branch only; the
  "already finalised" branch records nothing, and an injected history-write
  failure leaves the order retryable rather than finalized without history.

**Scheduler**
- prune deletes declarations of cancelled/expired orders and leaves active
  ones.

**Info event**
- tags absent when disabled and present only when enabled (`src/nip33.rs` tests next to
  `bond_policy_tags`), including the two threshold tags; `PayerHistorySettings`
  defaults `experienced_min_trades = 5`, `experienced_min_days = 30` when the
  keys are absent.

---

## 15. PR breakdown (atomic, backwards-compatible)

| ID | Repo | Scope | Depends on |
|---|---|---|---|
| **PH-0** | `mostro-core` | §6: actions, payloads, reasons, `verify()` arms, serde tests. Release. | — |
| **PH-1** | `mostrod` | Bump `mostro-core`; config section + helpers (§8.1–8.2); migration + `src/app/payer/db.rs` with unit tests (§9, §10.1). Nothing wired. | PH-0 |
| **PH-2** | `mostrod` | `declare_payer_action` + dispatch arm + tests (§10.2). | PH-1 |
| **PH-3** | `mostrod` | `fiat_sent` gate + push, `payment_history_action` + dispatch arm, `build_for_order` (§10.3–10.4). | PH-2 |
| **PH-4** | `mostrod` | `record_payer_success` + `payment_success` wiring + prune job (§10.5–10.6). | PH-1 (parallel with PH-2/3) |
| **PH-5** | `mostrod` | Info-event tags (§8.3), `docs/README.md` link, `ORDERS_AND_ACTIONS.md` row, Cashu boot warning (§13). | PH-1 |
| **PH-6** | `protocol` | New chapter `payer_declaration.md` (§6.5 examples, §7 canonicalisation registry with AR/EU/BR/… entries), `SUMMARY.md`, `message_suggestions_for_actions.md` reasons, `other_events.md` info tags. | PH-0 |

Critical path: PH-0 → PH-1 → PH-2 → PH-3. PH-4, PH-5 and PH-6 run in parallel
after PH-1 / PH-0. Each `mostrod` PR must keep the existing suite green
**unmodified** (tests may be added, not changed).

---

## 16. Definition of Done

- [ ] With `[payer_history]` absent, `cargo test` passes with no test edited,
      and `declare-payer` / `payment-history` answer `invalid_action`.
- [ ] With the feature on, the §5 flow works end-to-end against a test node:
      buyer declares, seller receives `payer-declared` and a `payment-history`
      push at `fiat-sent`, a second successful trade from the same account
      shows `successful_trades = 1`, `distinct_counterparties = 1`,
      `experienced_counterparties = 0` (the seller is below the thresholds).
- [ ] Seeding the seller with ≥ `experienced_min_trades` undisputed successes
      against *other* buyers, the oldest ≥ `experienced_min_days` days old,
      makes the next success report `experienced_counterparties = 1`; a seller
      whose prior successes were all with this same buyer stays at 0 (D-7),
      and a seller who crosses the thresholds only *after* the trade stays at
      0 until another success is recorded.
- [ ] A full-privacy buyer yields `buyer_mode = full_privacy` and writes no
      history rows (DB asserted).
- [ ] No new field appears on kinds 38383 / 38384; 38385 gains the §8.3
      tags only when `[payer_history].enabled = true` (`nip33` tests).
- [ ] `grep -rn "payment_hash" src/app/payer` shows no `tracing::` call that
      logs a hash at `info` or above.
- [ ] `cargo clippy --all-targets --all-features` clean; `cargo fmt` clean.
- [ ] Protocol book chapter merged and linked from `docs/README.md`.

---

## 17. Open questions (decide in review, not in code)

1. **D-6 scope.** Should a dispute resolved *in the buyer's favour* by a solver
   count as success? Current answer: no (conservative). Alternative: count it
   but not its counterparty.
2. **Post-success query.** §10.4 returns `not_found` after `Success`. If
   client authors need the "as seen at release time" snapshot for restored
   sessions, keep the declaration row until prune instead of consuming it,
   and use `payer_history.last_success_at < order.taken_at` — more state, more
   edge cases. Default: keep the simple rule.
3. **`require_declaration` granularity.** Global only in the MVP. Per-order
   (a maker flag on the 38383 event) would let sellers opt in individually
   but needs a new public tag; defer.

---

## 18. Future extensions (explicitly out of scope)

- **Hash-only history for full-privacy buyers** (`any user × hash`). Gives
  full-privacy buyers *some* signal but attributes the victim's own Mostro
  history to an attacker who targets a Mostro user; needs a separate risk
  analysis and a distinct `buyer_mode` value.
- **Admin RPC** to purge history for a `user_pubkey` / a `payment_hash`
  (retention policy, GDPR-style requests).
- **Economic weighting** (sats volume, age decay) and rotation warnings
  ("this buyer has used N distinct accounts in the last 30 days").
- **Cross-instance attestations** — must not introduce a global reusable
  identifier (gist §37).
- **Method-specific canonicalisation registry** as versioned data in the
  protocol repo so clients can update rules without a release.
- **Seller-side declaration** (symmetric protection for buyers against
  "pay to a third-party account" scams) — same tables, role-swapped.

---

## 19. Summary

The mechanism combines **sender verification** (done by the seller, assisted by
a hash forwarded through Mostro) with **payment-identity continuity** (four
aggregate counters computed by Mostro from successful trades only, one of them
a buyer-relative "was this counterparty already experienced?" snapshot — D-7).
On this codebase it maps to: three additive `mostro-core` variants, one
migration with three private tables, one new handler module, a short hook on
the single `Success` CAS, a prune job, four info-event tags and one protocol
chapter — all behind a flag that is off by default and leaves the daemon
byte-for-byte unchanged when disabled. A triangulation attacker can still make
the sender match; they cannot cheaply make `successful_trades = 47,
distinct_counterparties = 29, experienced_counterparties = 11,
first_success_at = nine months ago`.
