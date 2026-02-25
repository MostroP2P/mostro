# Fix: Rating Event Pubkey Format in Documentation

## Issue

[#572](https://github.com/MostroP2P/mostro/issues/572) — The documentation
(`SEPARATE_EVENT_KINDS_SPEC.md`) showed bech32-encoded `npub` format in the `d`
tag examples for Rating (kind 38384) and Info (kind 38385) events.

## Root Cause

The code implementation already uses hex-encoded pubkeys (via `PublicKey::to_string()`
from nostr-sdk, which outputs hex by default). The inconsistency was only in the
documentation examples.

## Changes

1. **Rating event example** (kind 38384): Changed `d` tag from `npub1abc123...`
   to `a1b2c3d4e5f6...` (hex format).
2. **Info event example** (kind 38385): Changed `d` tag from `npub1mostro...`
   to `a1b2c3d4e5f6...` (hex format).
3. **Added note** in the Kind Assignment table clarifying that all pubkeys in
   `d` tags use hex encoding for NIP-33 compatibility.

## Why Hex

- Nostr events internally use hex-encoded pubkeys.
- NIP-33 replaceable events use exact `d` tag matching for replacement.
- Mixing bech32 and hex would cause duplicate events and failed replacements.
- Relay filtering with `{"#d": ["<pubkey>"]}` requires exact format match.

## No Code Changes Required

The implementation in `src/nip33.rs` and `src/app/rate_user.rs` already passes
hex pubkeys via `PublicKey::to_string()`. This fix is documentation-only.
