# Mostro specification for clients

## Overview

All messages broadcasted by Mostro daemon are [Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/01.md#kinds) and use `38383` as event `kind`, a list of standard event kinds can be found [here](https://github.com/nostr-protocol/nips#event-kinds)

## Communication between users and Mostro

All messages from/to Mostro should be a Nostr event [kind 4](https://github.com/nostr-protocol/nips/blob/master/04.md), the `content` field of the event should be a base64-encoded, aes-256-cbc encrypted JSON-serialized string (with no white space or line breaks) of the following structure:

- `version`: Version of the protocol, currently `1`
- `pubkey` (optional): Real pubkey of the user, if present the message is signed with the real pubkey (TBD), this is used when users are sending messages from ephemeral keys
- [action](https://docs.rs/mostro-core/latest/mostro_core/enum.Action.html): Action to be performed by Mostro daemon
- [content](https://docs.rs/mostro-core/latest/mostro_core/enum.Content.html) (optional): Content of the message, this field is optional and depends on the action

These fields are relative to the wrapper, here an example of a `FiatSent` Order message, in this case `id` is the Order Id:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "npub1qqq...",
    "action": "FiatSent",
    "content": null
  }
}
```
