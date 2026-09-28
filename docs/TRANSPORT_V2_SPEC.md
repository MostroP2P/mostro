# Transport v2 — NIP-44 Direct Messaging (Protocol v2)

**Status:** Phases 0–3 done · Phase 4 (v0.19.0 cutover: remove protocol v1) in progress
**Issue:** [#626 — Messaging Transport Abstraction Layer](https://github.com/MostroP2P/mostro/issues/626)
**Cutover issue:** [#786 — run protocol v2 only in v0.19.0](https://github.com/MostroP2P/mostro/issues/786)
**Full proposal:** [issue comment](https://github.com/MostroP2P/mostro/issues/626#issuecomment-4694164653)
**Core implementation:** [mostro-core#152](https://github.com/MostroP2P/mostro-core/pull/152), released in mostro-core **0.13.0** (`transport` module)

## 1. Context and motivation

Mostro historically used NIP-59 Gift Wrap (kind `1059`) as its only wire
transport. Gift wraps give strong metadata privacy, but they are *opaque*:
the outer event is signed by a random throwaway key, so neither relays nor
the daemon can tell legitimate traffic from garbage without paying the full
decrypt cost. That makes Mostro vulnerable to a "Gift Wrap Apocalypse" —
spam floods that relays cannot rate-limit by sender and that force the
daemon to attempt NIP-44 decryption on every event (see the threat model in
issue #626).

The accepted direction (issue discussion): trade abuse-resistance for a
bounded amount of metadata. Mostro already rotates trade keys per trade —
the publicly exposed key for a given trade is short-lived, single-purpose
and never reused — so a *visible, rate-limitable* envelope leaks little,
while enabling:

- relay-side rate limiting by sender pubkey, and
- daemon-side cheap pre-validation **before** decrypting (Phase 2).

Protocol **v2** is that envelope: a signed kind-`14` event whose content is
NIP-44 encrypted. Protocol **v1** (gift wrap) is frozen and DEPRECATED.

## 2. Wire format (protocol v2)

### 2.1 Visible envelope

What relays and observers see:

```json
{
  "kind": 14,
  "pubkey": "<index N pubkey (trade key)>",
  "content": "<NIP-44 ciphertext>",
  "tags": [
    ["p", "<Mostro's pubkey>"],
    ["expiration", "<unix timestamp>"]
  ],
  "created_at": 1234567890,
  "sig": "<trade key signature>"
}
```

- **Author = trade key.** The event signature proves trade-key authorship
  (unlike v1, where the outer event is signed by a throwaway ephemeral key).
  This is what makes the transport rate-limitable and pre-filterable.
- **`expiration` (NIP-40):** trade messages are only relevant for the
  lifetime of a trade plus a dispute window, so they always carry an
  expiration tag (default 30 days, `dm_days` setting) instead of sitting on
  relays forever.
- **Mostro → user direction:** Mostro authors the event with its own
  well-known key, `p`-tagged to the user's trade key. Clients can subscribe
  with `authors=[mostro] AND #p=[trade keys]`.
- **NIP-17 deviation (deliberate):** NIP-17 defines kind 14 as an *unsigned*
  rumor that only travels inside a gift wrap. Mostro publishes it *signed*,
  because the author is an ephemeral single-trade key — the association the
  NIP-17 rule protects against is intentional and bounded. These events are
  not standard NIP-17 chats.

### 2.2 Encrypted content

The NIP-44 conversation key is derived from (trade key ↔ counterparty), so
only the two parties can decrypt. The plaintext is a JSON 3-element tuple —
v1's 2-tuple plus an identity proof:

```json
[
  { "order": { "version": 2, "...": "..." } },
  "<trade_sig | null>",
  ["<identity pubkey>", "<identity_sig>"]   // or null
]
```

| element | meaning |
|---|---|
| 1 | the logical `Message` (unchanged from v1, but `version: 2`) |
| 2 | trade key's `Message::sign` over the serialized first element, or `null` (Mostro's own messages are unsigned, as in v1) |
| 3 | identity proof `[identity_pubkey, identity_sig]`, or `null` for **full-privacy mode** (identity = trade key, mirroring v1's unsigned-rumor convention) |

### 2.3 Identity proof

In v1 the long-lived identity key is carried *authenticated* by the seal
(`identity = seal.pubkey`, hidden inside the wrap). v2 has no seal, so the
identity travels **inside the ciphertext** — never visible at the event
level, exactly as private as before — proven by a signature over the
domain-tagged payload:

```text
mostro-transport-v2-identity:<trade_pubkey_hex>:<message_json>
```

Including the trade pubkey binds the proof to the *specific trade key*
authoring the event (the binding v1 gets from the seal signature covering
the encrypted rumor). Signing the message JSON alone would let any party
that sees a plaintext tuple — the receiving node, or a compromised one —
graft the `(identity_pubkey, identity_sig)` pair onto an event authored by
a different trade key and have the identity misattributed. The receiver
recomputes the payload from `event.pubkey`, so a grafted proof fails
verification. (Found by review on mostro-core#152; regression-tested there.)

The signature scheme is the existing `Message::sign` /
`Message::verify_signature` (Schnorr over sha256). The identity key signs
once per message — the same custody model as v1, where it signs every seal.

## 3. Versioning

- mostro-core's `PROTOCOL_VER` is **2** (since 0.13.0).
- **v1** = gift wrap + 2-tuple, frozen. **v2** = kind-14 direct + 3-tuple.
- Which parser applies is keyed off the **event kind** (`1059` vs `14`),
  not the version field. mostro-core's `unwrap_incoming()` dispatches and
  returns the same `UnwrappedMessage` for both, which is why daemon
  handlers needed no changes.
- **Transitional (0.18.x only):** the inner `version` of the messages
  mostrod sends follows the configured transport — `1` on `gift-wrap`,
  `2` on `nip44` (`stamp_protocol_version` in `src/util.rs`, #785). From
  v0.19.0 there is no transport to follow, the stamping is removed and
  every message carries `PROTOCOL_VER` (2).

### 3.1 Compatibility — v1 and v2 are incompatible

Protocol v1 and protocol v2 **cannot talk to each other**. They differ in
the event kind, in who signs the outer event and in the shape of the
decrypted content, so:

- a v1 node subscribes to kind `1059` only and never sees a kind-`14`
  event; a v2 node subscribes to kind `14` only and never sees a gift wrap;
- there is no negotiation, fallback or translation between them — neither
  in the daemon nor on the wire;
- a mismatch is **silent**: the node does not answer and sends no
  `cant-do`, because the event never reaches a handler.

The only bridge is on the client side: the node advertises which protocol
it speaks in the `protocol_version` tag of its kind-`38385` instance-info
event (`"1"` or `"2"`), and a client reads it **before** it sends anything
to that node. A kind-`38385` event with no `protocol_version` tag comes
from a daemon older than v0.18.0, which speaks v1.

What each client does with that tag:

| client | reads `protocol_version` | v1 node | v2 node |
|---|---|---|---|
| Mostro Mobile (app v1, `MostroP2P/mobile`) ≥ v1.3.0 | yes, then picks the transport per node | works (gift wrap) | works (kind 14) |
| Mostro app v2 (`MostroP2P/app`) | yes, as a gate | **not supported** — the app tells the user the node speaks a protocol it does not | works (kind 14) |
| mostro-cli, mostrix | yes, then picks the transport per node | works | works |

So app v1 is the client that carries users across the transition: it keeps
both wrap paths and follows whatever the node announces. App v2 is
v2-native and never implements v1. Once every node runs v0.19.0 the v1
path in dual clients becomes dead code they can remove at their own pace.

## 4. Operator configuration — one transport per node (0.18.x)

There is **no dual mode**: a node speaks exactly one protocol version.
This section describes the 0.18.x settings; v0.19.0 removes the
`transport` setting altogether (§5, §6 Phase 4).

```toml
[mostro]
# "nip44" (protocol v2, default) | "gift-wrap" (protocol v1, DEPRECATED,
# explicit opt-in only)
transport = "nip44"

[expiration]
# kind-14 direct messages
dm_days = 30
```

| `transport` | event kind | who can trade on this node |
|---|---|---|
| `nip44` *(default)* | 14 (v2) | app v1 ≥ v1.3.0, app v2, mostro-cli, mostrix — the only mode from v0.19.0 |
| `gift-wrap` *(deprecated, explicit opt-in only)* | 1059 (v1) | app v1 and older v1-only clients; **not app v2** — wire behavior identical to pre-v2 daemons |

A node with no `transport` line starts in `nip44`. Operators who still need
to serve protocol-v1 clients must write `transport = "gift-wrap"` explicitly
in `settings.toml`; it is never selected automatically.

**Capability discovery:** the node advertises its protocol in the kind
`38385` instance-info event with a `protocol_version` tag (`"1"` or
`"2"`, derived from `transport`). Clients that predate the tag ignore it
and speak v1; every current client reads it before sending (§3.1).

Switching a community to v2 is a deliberate operator decision, coordinated
with the clients that community uses.

## 5. Release timeline

- **v0.18.0** — protocol v2 ships. Default `transport = "gift-wrap"`
  (nothing changes for existing clients). **Protocol v1 is DEPRECATED**:
  announced in release notes, protocol docs and the `protocol_version`
  tag. Client developers have the 0.18.x cycle to ship v2.
- **v0.18.5** — the daemon default flips to `transport = "nip44"`.
  `gift-wrap` stays fully functional but only as an explicit opt-in in
  `settings.toml` (the code path is untouched; removal is deferred to
  v0.19.0). The mostro-core `Transport::default()` remains `gift-wrap`
  for clients; mostrod overrides it with its own `default_transport()`.
- **v0.19.0** — protocol v2 is the **only** protocol mostrod speaks
  (#786). Every trace of v1 is removed from the daemon: the gift-wrap
  send and receive paths, the `transport` setting itself (not just its
  `"gift-wrap"` value), the per-transport version stamping (#785) and
  every v1-only test and doc. The info event keeps publishing
  `["protocol_version", "2"]`, now a constant: it is what app v1 and the
  other dual clients read to choose kind 14, and what app v2 checks
  before it talks to the node. mostro-core keeps its gift-wrap helpers
  for the dual clients until they drop v1 themselves; removing them is a
  separate mostro-core release, not part of this cutover.

## 6. Implementation phases

### Phase 0 — mostro-core (DONE — mostro-core#152, released 0.13.0)

The bulk of the work, all additive, in mostro-core's `transport` module:

- `wrap_message_nip44` / `unwrap_message_nip44` — the v2 wrap/unwrap pair
  (`Ok(None)` keeps its "not addressed to me" meaning).
- `unwrap_incoming` — kind dispatch returning the same `UnwrappedMessage`
  for both transports.
- `wrap_message_with` — send-side dispatcher.
- `Transport` enum — serde/`FromStr` for the config values, `event_kind()`,
  `protocol_version()`. Default `GiftWrap`.
- `PROTOCOL_VER` 1 → 2; v1 fixtures kept as parse-regression tests.
- Identity proof bound to the trade key via the domain-tagged payload
  (§2.3), with a grafting regression test.

### Phase 1 — mostrod wiring (DONE — #776)

Minimal daemon integration; **zero handler changes** by design:

- `mostro-core` 0.12.1 → **0.13.0**.
- `[mostro] transport` setting (`Transport`, serde default = `gift-wrap`
  at the time; `nip44` since v0.18.5) in `src/config/types.rs` +
  `settings.tpl.toml`.
- `[expiration] dm_days` knob (default 30) in `ExpirationSettings` and the
  `get_expiration_timestamp_for_kind` fallback (`DM_EVENT_KIND = 14` in
  `src/config/constants.rs`).
- `src/main.rs` — subscription filter uses `transport.event_kind()`.
- `src/app.rs` — event loop accepts only the configured kind and unwraps
  via `unwrap_incoming()`.
- `src/util.rs send_dm()` — wraps via `wrap_message_with(transport, …)`;
  on the nip44 transport, fills a default NIP-40 expiration from `dm_days`
  when the caller didn't pass one.
- `src/nip33.rs` — `protocol_version` tag in the kind-38385 info event.

### Phase 2 — anti-spam gates (DONE — #780; daemon-only, the payoff)

The reason v2 exists: reject junk *before* paying decrypt/parse costs. All of
the following are **v2-only** — the gate is skipped on the `gift-wrap`
transport, whose outer key is a throwaway with no pre-validatable signal.

- **Active-trade-pubkey cache** (`src/spam_gate.rs`, `SpamGate`): the trade
  keys that may legitimately message Mostro now — buyer/seller/creator of
  every non-terminal order, plus the solver of every active dispute. Built by
  `db::find_active_trade_pubkeys` (terminal set = the restore-session
  `EXCLUDED_ORDER_STATUSES` **minus `'dispute'`**, so disputed orders stay
  active). Warmed at startup in `main.rs` and rebuilt every
  `active_pubkeys_refresh_interval` seconds (default 60) by
  `scheduler::job_refresh_active_pubkeys` — a periodic full reload, chosen
  because status mutations are scattered across handlers with no single
  choke-point. Global-singleton (`OnceLock`), mirroring `PriceManager`.
- **Cheap pre-validation in the event loop** (`src/app.rs`), for kind 14,
  **before** `unwrap_incoming` decrypts: check `event.pubkey` against the
  cache.
- **Two lanes** — the necessary nuance to "only accept known keys": brand-new
  orders and takes arrive from keys Mostro has never seen.
  - *Known-keys lane:* sender in the cache → fast-path; only the base `pow`
    (already checked at the top of the loop) applies.
  - *First-contact lane:* sender unseen → must clear `pow_first_contact`
    (`[mostro]`, defaults to `pow` so existing configs are unchanged) before
    the daemon decrypts. This is where spam concentrates; PoW here plus
    relay-side rate limiting are the toll.
- **Dedup as defense in depth:** a `REPLAY_WINDOW_SECS` (60 s) guard drops a
  re-sent identical event id before decryption. The existing 10-second
  freshness window (post-decrypt, on the inner `created_at`) still applies as
  the precise stale-event check.
- **Validation order is load-bearing.** `accept_event` runs PoW → kind →
  **event signature** → gate (replay dedup, then the lane check) →
  `unwrap_incoming` (decrypt). The signature check must stay ahead of the
  gate: a nostr event id commits to `[0, pubkey, created_at, kind, tags,
  content]` and *not* to `sig`, so a copy of a victim's event with only `sig`
  tampered keeps the victim's id and its mined PoW. Recording ids before
  authentication would make the dedup a censorship primitive — the forged copy
  gets recorded, the genuine event that follows is dropped as a replay, and
  per the "no reply for dropped events" rule the sender never learns why.
  Verifying first costs one Schnorr check on events the gate would have
  dropped, and still runs entirely before the NIP-44 decrypt the gate exists
  to protect. The check is transport-agnostic and applies to v1 gift wraps
  too, where it is the daemon's *only* outer-event check: `unwrap_incoming`
  re-verifies the event on the v2 path alone, while `nip59::unwrap_message`
  verifies the seal's signature and never the outer wrap.
- **Discoverability** (`src/nip33.rs`): the kind-38385 info event carries a
  `pow_first_contact` tag next to `pow`. A dropped event gets no `cant-do`
  reply — it never reaches a handler — so the info event is the only way a
  client can learn what the first-contact lane costs. The published value is
  the *enforced* difficulty (`advertised_first_contact_pow`): on `nip44`
  `max(pow, effective_pow_first_contact())` — a max because the two checks run
  in sequence, so a `pow_first_contact` below `pow` still enforces `pow` — and
  on `gift-wrap` just `pow`, since the gate never runs there and advertising the
  stiffer number would make clients grind work nobody checks.
- **Known gap: recognition lags acceptance.** Accepting a create or take
  does *not* insert that trade key into the cache;
  `job_refresh_active_pubkeys` is the only writer after startup and rebuilds
  the whole set on its interval. Until the next rebuild — up to one
  `active_pubkeys_refresh_interval` (default 60 s), longer if a reload fails —
  the key is still treated as first contact, so a follow-up mined at `pow`
  (the invoice right after a take, for example) is silently dropped on a node
  whose `pow_first_contact` is above `pow`.

  The protocol rule stays the simple one: **only the event that introduces a
  trade key pays `pow_first_contact`**; everything after it pays `pow`. That is
  what the clients implement (app v2 and mostrix on new-order and take; app v1
  and mostro-cli do not read `pow_first_contact` at all), and asking clients to
  mine the higher difficulty on every event would only move a daemon bug into
  every client. The daemon closes the gap instead: when it accepts the event
  that ties a trade key to an order, it adds that key to the cache right away,
  and the periodic rebuild stays as the way keys leave the set. On nodes where
  `pow_first_contact` equals `pow` (the default) the gap has no effect.

New config (`[mostro]`): `pow_first_contact` (`Option<u8>`, default = `pow`)
and `active_pubkeys_refresh_interval` (default 60). Both `#[serde(default)]`,
so pre-Phase-2 `settings.toml` files are wire-identical. Zero handler changes;
the gate sits entirely in the event-loop preamble.

### Phase 3 — protocol docs + client migration (DONE)

- Protocol repo (`MostroP2P/protocol`): `overview.md` (both content
  tuples), `key_management.md` (the v2 wire format next to the gift-wrap
  walkthroughs) and `transport_migration.md` (the client developer guide:
  capability discovery, PoW and the first-contact gate, timeline).
- Clients: Mostro Mobile (app v1) since v1.3.0, mostro-cli and mostrix read
  `protocol_version` and speak whichever protocol the node announces; the
  v2 app (`MostroP2P/app`) speaks v2 only and refuses v1 nodes (§3.1).
- mostrod's default became `nip44` in v0.18.5 (#880).

### Phase 4 — the v0.19.0 cutover (IN PROGRESS — #786)

Remove every trace of protocol v1 from mostrod. The order is load-bearing:
the specs first, so that the code PRs have something to be checked against.

1. **Specs.** This document and the protocol repo's
   `transport_migration.md`: v1 and v2 are incompatible, how each client
   chooses (§3.1), and what v0.19.0 removes.
2. **Remove the `transport` setting.** Drop the field and its default from
   `src/config/types.rs` / `src/config/settings.rs`, the knob from
   `settings.tpl.toml` (a new install has no reason to see the field), and
   the gift-wrap deprecation warning in `src/main.rs`. The wire format is hardcoded to kind 14.
   A `settings.toml` that still carries the line is handled on purpose,
   not ignored by accident (the settings structs do not deny unknown
   fields):
   - `transport = "gift-wrap"` → mostrod **refuses to start** with an
     error that says v0.19.0 speaks protocol v2 only. That operator chose
     v1 explicitly; switching their community to another protocol behind
     their back is exactly the silent mismatch §3.1 describes.
   - `transport = "nip44"` → start normally and say nothing about it. The
     line is no longer needed, but it describes exactly what the node does,
     so there is nothing to warn about.

   The error for `gift-wrap` says why, and what to do: this version speaks
   protocol v2 only, clients on protocol v1 cannot use this node, remove the
   `transport` line to start on v2 or stay on 0.18.x to keep serving v1.
   Whatever the settings say, mostrod logs the protocol it speaks at startup
   (`protocol v2, event kind 14`), as it does today with the transport.
3. **Remove the v1 receive and send paths.** `src/main.rs` subscribes to
   kind 14 only; the event loop in `src/app.rs` accepts kind 14 only and
   unwraps with `unwrap_message_nip44` directly; `send_dm` in `src/util.rs`
   wraps with `wrap_message_nip44` and always sets the NIP-40 expiration.
4. **Revert the version stamping** (#785): delete `stamp_protocol_version`
   and its call in `send_dm`; every message carries `PROTOCOL_VER`.
5. **Simplify the anti-spam gate.** It no longer needs a transport check:
   it always runs. `advertised_first_contact_pow` in `src/nip33.rs` loses
   its gift-wrap arm, and tests that pin the "v2 only" behavior (for
   example `gate_applies_to_v2_only`) are rewritten or deleted.
6. **Keep the capability tag.** The info event publishes
   `["protocol_version", "2"]` as a constant. Removing it would make every
   node look like a pre-0.18.0 v1 daemon to the clients (§3.1).
7. **Tests and docs.** Delete every test marked `DEPRECATED(v0.19.0, #786)`
   and the v1 fixtures in mostrod (mostro-core keeps its own). Update
   `README.md`, `docs/STARTUP_AND_CONFIG.md`, and the sequence diagrams in
   `docs/ARCHITECTURE.md`, `docs/EVENT_ROUTING.md` and
   `docs/ORDERS_AND_ACTIONS.md`, which still say "GiftWrap".
8. **Release notes** for v0.19.0 say, in this order: v1 is gone; nodes that
   still run `transport = "gift-wrap"` will not start until the line is
   removed; users on that node need app v1 ≥ v1.3.0, app v2, mostro-cli or
   mostrix, all of which speak v2.

**Upgrading a v1 node.** Orders, trade keys and trade indexes are stored
without any notion of transport, so a trade that started on v1 continues
on v2 after the upgrade: the node answers on kind 14 and app v1 follows the
new `protocol_version` tag. Messages the node already sent as gift wraps
stay on the relays, and v0.19.0 no longer reads kind 1059, so a client that
restores a trade from relays needs its own v1 reader for the part that
happened before the upgrade. Operators should upgrade when few trades are
open and announce the change to their community first.

**Relay retention.** v2 messages carry a NIP-40 expiration (`dm_days`,
30 days by default) while v1 gift wraps from mostrod never did. Clients
that rebuild trades and disputes from relay history lose anything older
than `dm_days`. Operators whose disputes can last longer should raise
`dm_days` accordingly.

Out of scope for the cutover: transport metrics (message counts, decrypt
failures as a spam signal) and removing the gift-wrap helpers from
mostro-core.

## 7. Security notes

- **Identity privacy is unchanged from v1:** the identity pubkey only ever
  exists inside NIP-44 ciphertext readable by the two parties. What v2
  newly exposes is *activity* of an ephemeral trade key (who talks to
  Mostro, when, how much) — accepted, bounded by per-trade key rotation.
- **Identity proof grafting** is prevented by the trade-pubkey binding
  (§2.3). The trade signature (element 2) needs no domain tag because it is
  verified against `event.pubkey` — a foreign trade_sig under a different
  author fails by construction.
- **Event signature is load-bearing in v2** (it proves the visible sender):
  `unwrap_message_nip44` verifies it and hard-errors, unlike v1 where the
  outer signature is from a throwaway key and the seal carries the trust.
- The daemon's existing checks (PoW, 10-second freshness window, trade
  index, `identity != sender && signature.is_none()` bail-out) apply
  unchanged to both transports because both yield the same
  `UnwrappedMessage`.
