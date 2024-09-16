# Mostro specification for clients

## Overview

In order to have a shared order's book, Mostro daemon send [Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/01.md#kinds) with `38383` as event `kind`, you can find more details about that specific event [here](./order-event.md)

## Communication between users and Mostro

All messages from/to Mostro should be [Gift wrap Nostr events](https://github.com/nostr-protocol/nips/blob/master/59.md), the `content` of the `rumor` event should be a nip44 encrypted JSON-serialized string (with no white space or line breaks) of the following structure:

- `version`: Version of the protocol, currently `1`
- [action](https://docs.rs/mostro-core/latest/mostro_core/message/enum.Action.html): Action to be performed by Mostro daemon
- [content](https://docs.rs/mostro-core/latest/mostro_core/message/enum.Content.html) (optional): Content of the message, this field is optional and depends on the action

These fields are relative to the wrapper, here an example of a `fiat-sent` Order message, in this case `id` is the Order Id:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "0001be6bd50247846a28cce439a10470a39b1b6c81d5c3be2475156a413e1e3a",
    "action": "fiat-sent",
    "content": null
  }
}
```

## Keys

For all examples the participants will use this keys:

- Mostro's pubkey `dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a`
- Seller's ephemeral pubkey `00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78`
- Buyer's ephemeral pubkey `0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427`

## Ephemeral keys

Mostro clients should implement nip59 which creates newly fresh keys on each message to Mostro, the client will also creates newly keys for each order and is used to sign the seal, this pubkey will be linked to the order and discarded after the trade is done, this pubkey also will be used in case of a dispute.
