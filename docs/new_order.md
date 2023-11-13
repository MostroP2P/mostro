# Creating a new order

## Overview

All Mostro messages are [Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/01.md#kinds) and use `30078` as event `kind`, a list of standard event kinds can be found [here](https://github.com/nostr-protocol/nips#event-kinds)

## Communication between users and Mostro

All messages from/to Mostro should be a Nostr event [kind 4](https://github.com/nostr-protocol/nips/blob/master/04.md), the `content` field of the event should be a base64-encoded, aes-256-cbc encrypted JSON-serialized string (with no white space or line breaks) of the following structure:

- `version`
- `order_id` (optional)
- `pubkey` (optional)
- `action` (https://docs.rs/mostro-core/latest/mostro_core/enum.Action.html)
- `content` (optional https://docs.rs/mostro-core/latest/mostro_core/enum.Content.html)

## New order

To create a new sell order the user should send a Nostr event kind 4 to Mostro with the following content:

```json
{
  "version": "0",
  "pubkey": "npub1qqqxssz4k6swex94zdg5s4pqx3uqlhwsc2vdzvhjvzk33pcypkhqe9aeq2",
  "action": "Order",
  "content": {
    "Order": {
      "kind": "Sell",
      "status": "Pending",
      "amount": 0,
      "fiat_code": "VES",
      "fiat_amount": 100,
      "payment_method": "face to face",
      "premium": 1,
      "created_at": 0
    }
  }
}
```

Let's explain some of the fields:

- kind: `Sell` or `Buy`
- status: Is always `Pending` when creating a new order
- amount: 0 for when we want to sell with at market price, otherwise the amount in satoshis
- pubkey: Real user's pubkey, we use this when the message was sent from an ephemeral key
- created_at: No need to send the correct unix timestamp, Mostro will replace it with the current time

## Confirmation message

Mostro will send back a confirmation message to the user like the following:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "Order",
  "content": {
    "Order": {
      "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
      "kind": "Sell",
      "status": "Pending",
      "amount": 0,
      "fiat_code": "VES",
      "fiat_amount": 100,
      "payment_method": "face to face",
      "premium": 1,
      "created_at": 1698870173
    }
  }
}
```
