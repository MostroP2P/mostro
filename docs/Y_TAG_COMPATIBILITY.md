# `y` Tag Compatibility Contract

Mostro publishes a `y` tag on its public Nostr events to identify the platform. Since PR #653 the tag may carry a second value with the human-readable name of the Mostro instance, taken from `[mostro] name` in `settings.toml`. This document states the contract for that tag so indexers, aggregators, and clients can rely on it without breaking when the second value appears or disappears.

## Tag shape

This document talks about tag **values**. In a Nostr tag array, element `0` is the tag name (`"y"`) and the values start at index `1`.

Before #653 the tag always had exactly one value:

```json
["y", "mostro"]
```

Since #653 the tag has one or two values, depending on the instance configuration:

```json
["y", "mostro"]
["y", "mostro", "<instance name>"]
```

The second value is the `name` configured under `[mostro]` in `settings.toml`, trimmed of surrounding whitespace. When `name` is unset, empty, or whitespace only, the tag collapses back to the single-value form. The construction lives in `fn create_platform_tag_values` in `src/nip33.rs`.

## Where it is emitted

The `y` tag is present on every event built through the platform tag helper:

| Event | Kind | `z` value |
| --- | --- | --- |
| Order (NIP-33 replaceable) | 38383 | `order` |
| Instance info (NIP-33 replaceable) | 38385 | `info` |
| Dispute (NIP-33 replaceable) | 38386 | `dispute` |
| Dev fee audit | 8383 | `dev-fee-payment` |

Rating events (kind 38384) do **not** carry a `y` tag. Their tags come from `Rating::to_tags` in `mostro-core` and only include the rating fields plus `z`.

## Contract

- The first value is always the string `mostro`. It is the platform identifier and is the only value a consumer should use to recognize Mostro events.
- The second value is optional. It is present only when the instance operator configured a non-empty `[mostro] name`.
- The second value is additive metadata. Its presence, absence, or content never changes the meaning of the event.
- Consumers MUST NOT assume the tag has exactly one value.
- Consumers MUST NOT depend on the second value being present. A given instance can add, change, or remove its name between releases of the same event.
- Consumers that need to distinguish instances SHOULD key on the event `pubkey`, not on the instance name. The name is a display hint, not an identity.
- Mostro will not remove the first value or insert values before it. Any future value is appended after the instance name.

## Migration guidance for consumers

**You read only the first value.** No change needed. `tag[1] == "mostro"` keeps working in both shapes.

**You assume a fixed value count.** Code such as `if len(tag) != 2: reject` or a destructuring assignment that expects exactly `["y", platform]` fails on the two-value form. Relax the check to `len(tag) >= 2` and read `tag[1]` for the platform. Read `tag[2]` only if it exists:

```python
if len(tag) >= 2 and tag[0] == "y":
    platform = tag[1]
    instance_name = tag[2] if len(tag) >= 3 else None
```

**You filter with a relay `#y` filter.** NIP-01 states that relays index only the first value of a tag, so `{"#y": ["mostro"]}` matches both `["y", "mostro"]` and `["y", "mostro", "FreeSatoshi"]`. No change needed. The instance name is not indexed by relays, so `{"#y": ["<instance name>"]}` will not return that instance's events; use `authors` with the instance pubkey instead.

**You display the platform to users.** Prefer the instance name when present and fall back to `mostro`. Never show the tag as a single joined string, since the two values have different meanings.

## History

- Issue #649 proposed appending the instance name so aggregators can tell nodes apart without a pubkey lookup.
- PR #653 implemented it and added the platform tag helper with unit tests covering the empty, whitespace-only, and trimmed-name cases.
- First shipped in `v0.16.5`.

The public protocol specification documents the same shape in the [order event](https://github.com/MostroP2P/protocol/blob/main/src/order_event.md) and [other events](https://github.com/MostroP2P/protocol/blob/main/src/other_events.md) pages of the Mostro protocol book.
