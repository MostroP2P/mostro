# Disputes

## Overview

All Mostro messages are [Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/01.md#kinds) and use `30078` as event `kind`, a list of standard event kinds can be found [here](https://github.com/nostr-protocol/nips#event-kinds)

## Communication between users and Mostro

All messages from/to Mostro should be a Nostr event [kind 4](https://github.com/nostr-protocol/nips/blob/master/04.md), the `content` field of the event should be a base64-encoded, aes-256-cbc encrypted JSON-serialized string (with no white space or line breaks) of the following structure:

- `version`
- `order_id` (optional)
- `pubkey` (optional)
- `action` (https://docs.rs/mostro-core/latest/mostro_core/enum.Action.html)
- `content` (optional https://docs.rs/mostro-core/latest/mostro_core/enum.Content.html)

## Start a dispute

A use can start a dispute in an order with status `Pending` or `FiatSent` sending action `Dispute`, here is an example where the seller initiates a dispute:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": "npub1qqqt938cer4dvlslg04zwwf66ts8r3txp6mv79cx2498pyuqx8uq0c7qkj",
  "action": "Dispute",
  "content": null
}
```

## Mostro response

Mostro will send this message to the seller:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "DisputeInitiatedByYou,",
  "content": null
}
```

And here is the message to the buyer:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "DisputeInitiatedByPeer",
  "content": null
}
```
