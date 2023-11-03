# Mostro specification for clients

## Overview

All messages broadcasted by Mostro daemon are [Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/01.md#kinds) and use `30078` as event `kind`, a list of standard event kinds can be found [here](https://github.com/nostr-protocol/nips#event-kinds)

## Communication between users and Mostro

All messages from/to Mostro should be a Nostr event [kind 4](https://github.com/nostr-protocol/nips/blob/master/04.md), the `content` field of the event should be a base64-encoded, aes-256-cbc encrypted JSON-serialized string (with no white space or line breaks) of the following structure:

This is version 0 of the protocol, the version is specified in the `version` field of the message, and will be replaced by a new version soon.

- `version`
- `order_id` (optional)
- `pubkey` (optional)
- [`action`](https://docs.rs/mostro-core/latest/mostro_core/enum.Action.html)
- `content`[optional](https://docs.rs/mostro-core/latest/mostro_core/enum.Content.html)
