# Transport v2 — NIP-44 Direct Messaging (Protocol v2)

**Status:** Phases 1–2 implemented · Phases 3–4 pending
**Issue:** [#626 — Messaging Transport Abstraction Layer](https://github.com/MostroP2P/mostro/issues/626)
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

- `Message.version` is **2** (mostro-core `PROTOCOL_VER`, since 0.13.0).
- **v1** = gift wrap + 2-tuple, frozen. **v2** = kind-14 direct + 3-tuple.
- Which parser applies is keyed off the **event kind** (`1059` vs `14`),
  not the version field. mostro-core's `unwrap_incoming()` dispatches and
  returns the same `UnwrappedMessage` for both, which is why daemon
  handlers needed no changes.

## 4. Operator configuration — one transport per node

There is **no dual mode**: a node speaks exactly one protocol version.

```toml
[mostro]
# "gift-wrap" (protocol v1, DEPRECATED) | "nip44" (protocol v2)
transport = "gift-wrap"

[expiration]
# kind-14 direct messages
dm_days = 30
```

| `transport` | event kind | who can trade on this node |
|---|---|---|
| `gift-wrap` *(default in 0.18.x)* | 1059 (v1) | every current client — wire behavior identical to pre-v2 daemons |
| `nip44` | 14 (v2) | v2-capable clients only — the only mode from v0.19.0 |

**Capability discovery:** the node advertises its protocol in the kind
`38385` instance-info event with a `protocol_version` tag (`"1"` or
`"2"`, derived from `transport`). Old clients ignore the unknown tag;
v2-capable clients check it and use the matching wire format — a client
implementation should keep both wrap paths (mostro-core ships both) to
talk to v1 and v2 nodes during the transition.

Switching a community to v2 is a deliberate operator decision, coordinated
with the clients that community uses.

## 5. Release timeline

- **v0.18.0** — protocol v2 ships. Default `transport = "gift-wrap"`
  (nothing changes for existing clients). **Protocol v1 is DEPRECATED**:
  announced in release notes, protocol docs and the `protocol_version`
  tag. Client developers have the 0.18.x cycle to ship v2.
- **v0.19.0** — protocol v2 becomes the default and only protocol.
  Everything v1-related is removed from mostrod (gift-wrap path,
  `"gift-wrap"` setting value, v1 acceptance). mostro-core keeps its
  gift-wrap helpers for clients' own migration needs.

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

### Phase 1 — mostrod wiring (DONE — this change)

Minimal daemon integration; **zero handler changes** by design:

- `mostro-core` 0.12.1 → **0.13.0**.
- `[mostro] transport` setting (`Transport`, serde default = `gift-wrap`)
  in `src/config/types.rs` + `settings.tpl.toml`.
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

### Phase 2 — anti-spam gates (DONE — this change; daemon-only, the payoff)

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

New config (`[mostro]`): `pow_first_contact` (`Option<u8>`, default = `pow`)
and `active_pubkeys_refresh_interval` (default 60). Both `#[serde(default)]`,
so pre-Phase-2 `settings.toml` files are wire-identical. Zero handler changes;
the gate sits entirely in the event-loop preamble.

### Phase 3 — protocol docs + client migration (PENDING)

- Update the protocol repo (`MostroP2P/protocol`): `overview.md` ("The
  Message": both transports, the v2 tuple, `version: 2`),
  `key_management.md` (v2 examples mirroring the existing unencrypted
  gift-wrap walkthroughs), migration guide for client developers.
- mostro-cli / client support via the same mostro-core 0.13.0 APIs:
  clients keep both wrap paths and pick per node from `protocol_version`.

### Phase 4 — the v0.19.0 cutover (PENDING)

- Default `transport = "nip44"`; remove the v1 path from mostrod entirely
  (per §5). Metrics: `messages_received_total`, decrypt failures as a spam
  indicator.

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
