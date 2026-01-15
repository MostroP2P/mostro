# Separate Event Kinds Technical Specification

## Overview

This document specifies the implementation of separate Nostr event kinds for different Mostro event types. Currently, all Mostro application events (orders, ratings, info, disputes) use a single replaceable event kind (`38383`), differentiated only by the `z` tag. This change introduces dedicated kinds for each event type to improve client efficiency and relay filtering.

## Problem Statement

### Current Architecture

All Mostro events currently use `NOSTR_REPLACEABLE_EVENT_KIND` (kind `38383`) from `mostro_core`:

| Event Type | Current Kind | Identifier (`z` tag) |
|------------|-------------|---------------------|
| Orders | 38383 | `"order"` |
| Ratings | 38383 | (embedded in order events) |
| Info | 38383 | `"info"` |
| Disputes | 38383 | `"dispute"` |

### Issues

1. **Inefficient client subscriptions**: Clients wanting only orders must download all event types
2. **Increased bandwidth usage**: Unnecessary data transfer for specialized clients (e.g., orderbook displays)
3. **Relay filtering overhead**: Relays cannot efficiently filter by event type at the protocol level
4. **Tag parsing requirement**: Clients must parse `z` tags to determine event type after download

## Proposed Solution

### New Kind Assignment

| Event Type | New Kind | Change | NIP-33 Identifier (`d` tag) |
|------------|----------|--------|----------------------------|
| Orders | 38383 | No change | `order.id` (UUID) |
| Ratings | 38384 | New (+1) | `user_pubkey` |
| Info | 38385 | New (+2) | `mostro_pubkey` |
| Disputes | 38386 | New (+3) | `dispute.id` (UUID) |

All kinds remain in the NIP-33 replaceable event range (30000-39999), ensuring events can be updated by publishing new events with the same `d` tag.

## Implementation Details

### Phase 1: Define New Constants in `mostro_core`

**Location**: `mostro_core` crate (external dependency)

**Changes Required**:

```rust
// Current constant (unchanged)
pub const NOSTR_REPLACEABLE_EVENT_KIND: u16 = 38383;

// New constants to add
pub const NOSTR_RATING_EVENT_KIND: u16 = 38384;
pub const NOSTR_INFO_EVENT_KIND: u16 = 38385;
pub const NOSTR_DISPUTE_EVENT_KIND: u16 = 38386;
```

**Rationale**: Constants defined in `mostro_core` ensure consistency across all Mostro components (daemon, clients, libraries).

### Phase 2: Update `src/nip33.rs`

#### 2.1 Create Kind-Specific Event Builders

**Current Implementation** (`src/nip33.rs:24-38`):

```rust
pub fn new_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
) -> Result<Event, Error> {
    // Uses NOSTR_REPLACEABLE_EVENT_KIND for all events
    EventBuilder::new(nostr::Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND), content)
        .tags(tags)
        .sign_with_keys(keys)
}
```

**Proposed Changes**:

Option A - Add kind parameter to existing function:

```rust
pub fn new_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
    kind: u16,  // New parameter
) -> Result<Event, Error> {
    let mut tags: Vec<Tag> = Vec::with_capacity(1 + extra_tags.len());
    tags.push(Tag::identifier(identifier));
    tags.extend(extra_tags);
    let tags = Tags::from_list(tags);

    EventBuilder::new(nostr::Kind::Custom(kind), content)
        .tags(tags)
        .sign_with_keys(keys)
}
```

Option B - Create separate functions for each event type (recommended for clarity):

```rust
/// Creates a new order event (kind 38383)
pub fn new_order_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
) -> Result<Event, Error> {
    create_event(keys, content, identifier, extra_tags, NOSTR_REPLACEABLE_EVENT_KIND)
}

/// Creates a new rating event (kind 38384)
pub fn new_rating_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
) -> Result<Event, Error> {
    create_event(keys, content, identifier, extra_tags, NOSTR_RATING_EVENT_KIND)
}

/// Creates a new info event (kind 38385)
pub fn new_info_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
) -> Result<Event, Error> {
    create_event(keys, content, identifier, extra_tags, NOSTR_INFO_EVENT_KIND)
}

/// Creates a new dispute event (kind 38386)
pub fn new_dispute_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
) -> Result<Event, Error> {
    create_event(keys, content, identifier, extra_tags, NOSTR_DISPUTE_EVENT_KIND)
}

// Internal helper function
fn create_event(
    keys: &Keys,
    content: &str,
    identifier: String,
    extra_tags: Tags,
    kind: u16,
) -> Result<Event, Error> {
    let mut tags: Vec<Tag> = Vec::with_capacity(1 + extra_tags.len());
    tags.push(Tag::identifier(identifier));
    tags.extend(extra_tags);
    let tags = Tags::from_list(tags);

    EventBuilder::new(nostr::Kind::Custom(kind), content)
        .tags(tags)
        .sign_with_keys(keys)
}
```

### Phase 3: Update Event Publishing Locations

#### 3.1 Order Events

**Location**: `src/nip33.rs` - `order_to_tags()` function and callers

**Current Usage**: Events created via `new_event()` with order tags

**Change**: Use `new_order_event()` (or `new_event()` with `NOSTR_REPLACEABLE_EVENT_KIND`)

**Files to Update**:
- `src/util.rs` - `send_new_order_msg()` and related functions
- Any location calling `new_event()` for order publishing

#### 3.2 Rating Events

**Location**: `src/util.rs:606-633` - `update_user_rating_event()`

**Current Code**:
```rust
let event = new_event(keys, "", user.to_string(), tags)?;
```

**Updated Code**:
```rust
let event = new_rating_event(keys, "", user.to_string(), tags)?;
```

#### 3.3 Info Events

**Location**: `src/scheduler.rs` - `job_info_event_send()`

**Current Flow**:
1. `info_to_tags()` creates tags with `z: "info"`
2. Event created with `new_event()` using mostro pubkey as identifier

**Updated Flow**:
1. `info_to_tags()` unchanged (tags still useful for filtering)
2. Event created with `new_info_event()`

#### 3.4 Dispute Events

**Location**: `src/app/dispute.rs:20-82` - `publish_dispute_event()`

**Current Code** (line 57):
```rust
let event = new_event(my_keys, "", dispute.id.to_string(), tags)
    .map_err(|_| MostroInternalErr(ServiceError::DisputeEventError))?;
```

**Updated Code**:
```rust
let event = new_dispute_event(my_keys, "", dispute.id.to_string(), tags)
    .map_err(|_| MostroInternalErr(ServiceError::DisputeEventError))?;
```

### Phase 4: Update Tag Structure

The `z` tag should be retained for backwards compatibility and additional context, but is no longer the primary identifier of event type.

| Event Type | Kind | `z` tag value |
|------------|------|---------------|
| Orders | 38383 | `"order"` |
| Ratings | 38384 | `"rating"` |
| Info | 38385 | `"info"` |
| Disputes | 38386 | `"dispute"` |

**Note**: Rating events currently don't have a `z` tag. This implementation should add one for consistency.

### Phase 5: Update Callers

#### Files Requiring Updates

| File | Function | Current Usage | New Usage |
|------|----------|---------------|-----------|
| `src/nip33.rs` | `new_event()` | Generic event creation | Keep for orders or deprecate |
| `src/util.rs` | `update_user_rating_event()` | `new_event()` | `new_rating_event()` |
| `src/util.rs` | Order publishing functions | `new_event()` | `new_order_event()` |
| `src/scheduler.rs` | `job_info_event_send()` | `new_event()` | `new_info_event()` |
| `src/app/dispute.rs` | `publish_dispute_event()` | `new_event()` | `new_dispute_event()` |

## Migration Strategy

### Backwards Compatibility

To ensure smooth transition for existing clients:

1. **Transition Period**: Publish events with both old and new kinds during migration
2. **Client Updates**: Clients should update to query new kinds
3. **Deprecation Notice**: Document that kind 38383 will only be used for orders after transition

### Transition Implementation (Optional)

During transition, publish duplicate events:

```rust
// Temporary: publish both old and new format
let legacy_event = new_event(keys, content, identifier.clone(), tags.clone())?;
let new_event = new_dispute_event(keys, content, identifier, tags)?;

client.send_event(&legacy_event).await?;
client.send_event(&new_event).await?;
```

**Note**: This doubles event publishing and should only be used if backwards compatibility is critical. Recommended approach is a clean cutover with client coordination.

## Event Examples

### Order Event (Kind 38383 - Unchanged)

```json
{
  "kind": 38383,
  "content": "",
  "tags": [
    ["d", "550e8400-e29b-41d4-a716-446655440000"],
    ["k", "sell"],
    ["f", "USD"],
    ["s", "pending"],
    ["amt", "100000"],
    ["fa", "100"],
    ["pm", "bank_transfer,paypal"],
    ["premium", "5"],
    ["network", "mainnet"],
    ["layer", "lightning"],
    ["expires_at", "1704067200"],
    ["expiration", "1704153600"],
    ["y", "mostro"],
    ["z", "order"]
  ]
}
```

### Rating Event (Kind 38384 - New)

```json
{
  "kind": 38384,
  "content": "",
  "tags": [
    ["d", "npub1abc123..."],
    ["total_reviews", "42"],
    ["total_rating", "4.8"],
    ["last_rating", "5"],
    ["min_rating", "3"],
    ["max_rating", "5"],
    ["y", "mostro"],
    ["z", "rating"]
  ]
}
```

### Info Event (Kind 38385 - New)

```json
{
  "kind": 38385,
  "content": "",
  "tags": [
    ["d", "npub1mostro..."],
    ["mostro_version", "0.15.6"],
    ["mostro_commit_hash", "abc123"],
    ["max_order_amount", "1000000"],
    ["min_order_amount", "1000"],
    ["fee", "0.01"],
    ["pow", "0"],
    ["y", "mostro"],
    ["z", "info"]
  ]
}
```

### Dispute Event (Kind 38386 - New)

```json
{
  "kind": 38386,
  "content": "",
  "tags": [
    ["d", "660e8400-e29b-41d4-a716-446655440001"],
    ["s", "pending"],
    ["initiator", "buyer"],
    ["y", "mostro"],
    ["z", "dispute"]
  ]
}
```

## Client Query Examples

### Before (Single Kind, Tag Filtering)

```javascript
// Get only orders - requires downloading all events
const filter = {
  kinds: [38383],
  "#y": ["mostro"],
  "#z": ["order"]  // Must filter after download
};
```

### After (Dedicated Kinds)

```javascript
// Get only orders - efficient relay-level filtering
const ordersFilter = { kinds: [38383], "#y": ["mostro"] };

// Get only ratings
const ratingsFilter = { kinds: [38384], "#y": ["mostro"] };

// Get only info
const infoFilter = { kinds: [38385], "#y": ["mostro"] };

// Get only disputes
const disputesFilter = { kinds: [38386], "#y": ["mostro"] };

// Get all Mostro events (if needed)
const allFilter = { kinds: [38383, 38384, 38385, 38386], "#y": ["mostro"] };
```

## Testing Plan

### Unit Tests

1. **Event Creation Tests**:
   - Verify `new_order_event()` creates kind 38383
   - Verify `new_rating_event()` creates kind 38384
   - Verify `new_info_event()` creates kind 38385
   - Verify `new_dispute_event()` creates kind 38386

2. **Tag Preservation Tests**:
   - Verify all existing tags are preserved
   - Verify `z` tag is present and correct for each type

### Integration Tests

1. **Event Publishing**:
   - Create and publish each event type
   - Query relay by specific kind
   - Verify only matching events returned

2. **NIP-33 Replacement**:
   - Publish event with same `d` tag
   - Verify old event is replaced
   - Verify replacement works per-kind

### Manual Testing Checklist

- [ ] Create new order, verify kind 38383 on relay
- [ ] Complete order with rating, verify kind 38384 on relay
- [ ] Wait for info event, verify kind 38385 on relay
- [ ] Open dispute, verify kind 38386 on relay
- [ ] Query each kind separately, verify correct events returned
- [ ] Update existing order, verify NIP-33 replacement works

## Dependencies

### External Changes Required

1. **mostro_core**: Add new kind constants
   - `NOSTR_RATING_EVENT_KIND = 38384`
   - `NOSTR_INFO_EVENT_KIND = 38385`
   - `NOSTR_DISPUTE_EVENT_KIND = 38386`

### Client Updates Required

1. **mostro-cli**: Update subscription filters
2. **mostro-web**: Update relay queries
3. **Third-party clients**: Documentation for migration

## Rollout Plan

### Step 1: Update mostro_core
- Add new constants
- Release new version

### Step 2: Update mostrod
- Implement new event functions
- Update all publishing locations
- Test thoroughly

### Step 3: Update Clients
- Update mostro-cli queries
- Update mostro-web queries
- Publish migration guide

### Step 4: Deploy
- Deploy updated mostrod
- Monitor relay events
- Verify correct kinds being published

### Step 5: Cleanup (Optional)
- Remove backwards compatibility code if implemented
- Update documentation

## Files to Modify Summary

| File | Changes |
|------|---------|
| `mostro_core/src/lib.rs` (external) | Add 3 new kind constants |
| `src/nip33.rs` | Add kind-specific event builders |
| `src/util.rs` | Update rating event publishing |
| `src/scheduler.rs` | Update info event publishing |
| `src/app/dispute.rs` | Update dispute event publishing |
| `docs/ARCHITECTURE.md` | Document new kinds |

## Verification

After implementation, verify correct operation with:

```bash
# Query orders only
nostr-cli req -k 38383 --tag y=mostro

# Query ratings only
nostr-cli req -k 38384 --tag y=mostro

# Query info only
nostr-cli req -k 38385 --tag y=mostro

# Query disputes only
nostr-cli req -k 38386 --tag y=mostro
```

## References

- [NIP-01: Basic Protocol](https://github.com/nostr-protocol/nips/blob/master/01.md) - Event kinds
- [NIP-33: Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/33.md) - Replaceable events
- [NIP-69: Peer-to-peer Order Events](https://github.com/nostr-protocol/nips/blob/master/69.md) - P2P marketplace specification
- Current implementation: `src/nip33.rs`, `src/app/dispute.rs`, `src/util.rs`
