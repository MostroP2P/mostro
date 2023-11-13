# User rating

## Overview

All Mostro messages are [Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/01.md#kinds) and use `30078` as event `kind`, a list of standard event kinds can be found [here](https://github.com/nostr-protocol/nips#event-kinds)

## Communication between users and Mostro

All messages from/to Mostro should be a Nostr event [kind 4](https://github.com/nostr-protocol/nips/blob/master/04.md), the `content` field of the event should be a base64-encoded, aes-256-cbc encrypted JSON-serialized string (with no white space or line breaks) of the following structure:

- `version`
- `order_id` (optional)
- `pubkey` (optional)
- `action` (https://docs.rs/mostro-core/latest/mostro_core/enum.Action.html)
- `content` (optional https://docs.rs/mostro-core/latest/mostro_core/enum.Content.html)

After a successful trade Mostro send a nip04 event to both parties to let them know they can rate each other, here an example how the message look like:

```json
{
  "version": "0",
  "order_id": "7e44aa5d-855a-4b17-865e-8ca3834a91a3",
  "pubkey": null,
  "action": "RateUser",
  "content": null
}
```

After a Mostro client receive this message, the user can rate the other party, the rating is a number between 1 and 5, to rate the client must receive user's input and create a new nip04 event to send to Mostro with this content:

```json
{
  "version": "0",
  "order_id": "7e44aa5d-855a-4b17-865e-8ca3834a91a3",
  "pubkey": null,
  "action": "RateUser",
  "content": {
    "RatingUser": 5 // User input
  }
}
```

## Confirmation message

If Mostro received the message correct it will send back a last confirmation message to the user with `Action: Received`:

```json
{
  "version": "0",
  "order_id": "7e44aa5d-855a-4b17-865e-8ca3834a91a3",
  "pubkey": null,
  "action": "Received",
  "content": null
}
```
