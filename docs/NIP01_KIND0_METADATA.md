# NIP-01 Kind 0 Metadata Event

Mostro now publishes a NIP-01 kind 0 metadata event on startup, allowing Nostr clients to display the instance's profile information (name, description, avatar, and website).

## Background

Mostro already published several Nostr events on startup and periodically:

- **Kind 10002** (NIP-65 Relay List) — via the scheduler
- **Kind 38385** (Mostro Info) — via the scheduler
- **Kind 38383** (Orders) — as order events

However, there was no **NIP-01 kind 0 metadata event**. This is the standard Nostr profile mechanism — every relay-aware client already knows how to fetch and display kind 0 metadata. Without it, clients could not show the Mostro instance's name, description, avatar, or website.

## What Was Implemented

A kind 0 metadata event is now published once on each startup, after the Nostr client connects and subscribes but before the LND connector initializes. This event is a **replaceable event** (per NIP-01), so relays keep only the latest version, ensuring the profile stays fresh across restarts.

The implementation:

1. Reads four optional configuration fields from `[mostro]` settings
2. Builds a `nostr_sdk::Metadata` object with any configured fields
3. Signs the event with the Mostro keypair
4. Publishes to all connected relays

If no metadata fields are configured, no event is published.

## NIP-01 Kind 0 Specification

Per [NIP-01](https://github.com/nostr-protocol/nips/blob/master/01.md), a kind 0 event's `content` field contains a stringified JSON object:

```json
{
  "name": "Mostro P2P",
  "about": "A peer-to-peer Bitcoin trading daemon over the Lightning Network",
  "picture": "https://example.com/mostro-avatar.png",
  "website": "https://mostro.network"
}
```

## Configuration

Four optional fields were added to the `[mostro]` section in `settings.toml`:

```toml
[mostro]
# NIP-01 Kind 0 Metadata (optional)
# Human-readable name for this Mostro instance
name = "Mostro P2P"
# Short description of this Mostro instance
about = "A peer-to-peer Bitcoin trading daemon over the Lightning Network"
# URL to avatar image (recommended: square, max 128x128px)
picture = "https://example.com/mostro-avatar.png"
# Operator website URL
website = "https://mostro.network"
```

All fields are `Option<String>` and default to `None`. The template (`settings.tpl.toml`) includes these fields commented out, so they are inactive by default.

### Picture Size Recommendations

The `picture` field should point to a small, square image:

- **Maximum dimensions:** 128x128 pixels
- **Format:** PNG or JPEG preferred
- **Rationale:** Nostr clients typically display profile pictures as small avatars. Larger images waste bandwidth and relay storage. Some relays may reject events with excessively large content.

## Files Changed

| File | Change |
|------|--------|
| `src/config/types.rs` | Added `name`, `about`, `picture`, `website` fields to `MostroSettings` and its `Default` impl |
| `settings.tpl.toml` | Added commented-out template entries for the four metadata fields |
| `src/main.rs` | Added kind 0 metadata event publishing after Nostr client subscription |

## Boot Sequence Position

The metadata event is published at boot step 4, after the Nostr client connects (step 3) and before LND initialization (step 5). See [STARTUP_AND_CONFIG.md](STARTUP_AND_CONFIG.md) for the full boot sequence.
