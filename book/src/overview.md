# Mostro specification for clients

## Overview

All messages broadcasted by Mostro daemon are [Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/01.md#kinds) and use `38383` as event `kind`, a list of standard event kinds can be found [here](https://github.com/nostr-protocol/nips#event-kinds)

## Communication between users and Mostro

All messages from/to Mostro should be a Nostr event [kind 4](https://github.com/nostr-protocol/nips/blob/master/04.md), the `content` field of the event should be a base64-encoded, aes-256-cbc encrypted JSON-serialized string (with no white space or line breaks) of the following structure:

- `version`: Version of the protocol, currently `1`
- `pubkey` (optional): Real pubkey of the user, if present the message is signed with the real pubkey, this is used when users are sending messages from ephemeral keys
- [action](https://docs.rs/mostro-core/latest/mostro_core/enum.Action.html): Action to be performed by Mostro daemon
- [content](https://docs.rs/mostro-core/latest/mostro_core/enum.Content.html) (optional): Content of the message, this field is optional and depends on the action

These fields are relative to the wrapper, here an example of a `FiatSent` Order message, in this case `id` is the Order Id:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "0001be6bd50247846a28cce439a10470a39b1b6c81d5c3be2475156a413e1e3a",
    "action": "FiatSent",
    "content": null
  }
}
```

## Keys

For all examples the participants will use this keys:

- Mostro's pubkey `dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a`
- Seller's real pubkey `00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78`
- Seller's ephemeral pubkey `1f5bb148a25bca31506594722e746b10acf2641a12725b12072dcbc46ade544d`
- Buyer's real pubkey `0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427`
- Buyer's ephemeral pubkey `9a42ac72d6466a6dbe5b4b07a8717ee13e55abb6bdd810ea9c321c9a32ee837b`

## Ephemeral keys

Mostro clients should use newly fresh keys to communicate with Mostro, indicating the pubkey where they want to be contacted by the counterpart in the `pubkey` field of the message, this way orders and users can't be easily linked, `buyer_pubkey` and `seller_pubkey` fields are each party real pubkeys.
