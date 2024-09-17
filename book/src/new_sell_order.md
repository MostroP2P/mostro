# Creating a new sell order

To create a new sell order the user should send a Gift wrap Nostr event to Mostro, the rumor event should have the following rumor's content:

```json
{
  "order": {
    "version": 1,
    "action": "new-order",
    "content": {
      "order": {
        "kind": "sell",
        "status": "pending",
        "amount": 0,
        "fiat_code": "VES",
        "min_amount": null,
        "max_amount": null,
        "fiat_amount": 100,
        "payment_method": "face to face",
        "premium": 1,
        "created_at": 0
      }
    }
  }
}
```

Let's explain some of the fields:

- kind: `sell` or `buy`
- status: Is always `pending` when creating a new order
- amount: 0 for when we want to sell with at market price, otherwise the amount in satoshis
- created_at: No need to send the correct unix timestamp, Mostro will replace it with the current time

The event to send to Mostro would look like this:

```json
{
  "id": "cade205b849a872d74ba4d2a978135dbc05b4e5f483bb4403c42627dfd24f67d",
  "kind": 1059,
  "pubkey": "1f5bb148a25bca31506594722e746b10acf2641a12725b12072dcbc46ade544d", // Seller's ephemeral pubkey
  "content": "sealed-rumor-content",
  "tags": [
    ["p", "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a"] // Mostro's pubkey
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Confirmation message

Mostro will send back a nip59 event as a confirmation message to the user like the following (unencrypted rumor's content example):

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78",
    "content": {
      "order": {
        "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
        "kind": "sell",
        "status": "pending",
        "amount": 0,
        "fiat_code": "VES",
        "fiat_amount": 100,
        "payment_method": "face to face",
        "premium": 1,
        "created_at": 1698870173
      }
    }
  }
}
```

Mostro publishes this order as an event kind `38383` with status `pending`:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "84fad0d29cb3529d789faeff2033e88fe157a48e071c6a5d1619928289420e31",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1702548701,
    "kind": 38383,
    "tags": [
      ["d", "ede61c96-4c13-4519-bf3a-dcf7f1e9d842"],
      ["k", "sell"],
      ["f", "VES"],
      ["s", "pending"],
      ["amt", "0"],
      ["fa", "100"],
      ["pm", "face to face", "bank transfer"],
      ["premium", "1"],
      ["network", "mainnet"],
      ["layer", "lightning"],
      ["expiration", "1719391096"],
      ["y", "mostrop2p"],
      ["z", "order"]
    ],
    "content": "",
    "sig": "7e8fe1eb644f33ff51d8805c02a0e1a6d034e6234eac50ef7a7e0dac68a0414f7910366204fa8217086f90eddaa37ded71e61f736d1838e37c0b73f6a16c4af2"
  }
]
```
