# Creating a new sell range order

To create a new range order the user should send a Nostr event kind 4 (an encrypted message) to Mostro with the following content:

```json
{
  "order": {
    "version": 1,
    "pubkey": "00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78", // Seller's real pubkey
    "action": "new-order",
    "content": {
      "order": {
        "kind": "sell",
        "status": "pending",
        "amount": 0,
        "fiat_code": "VES",
        "min_amount": 10,
        "max_amount": 20,
        "fiat_amount": 0,
        "payment_method": "face to face",
        "premium": 1,
        "created_at": 0
      }
    }
  }
}
```

We two new fields, `min_amount` and `max_amount`, to define the range of the order. The `fiat_amount` field is set to 0 to indicate that the order is for a range of amounts.

When a taker takes the order, the amount will be set on the message.

## Confirmation message

Mostro will send back a nip04 event as a confirmation message to the user like the following:

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
        "min_amount": 10,
        "max_amount": 20,
        "fiat_amount": 0,
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
      ["fa", "10", "20"],
      ["pm", "face to face"],
      ["premium", "1"],
      ["y", "mostrop2p"],
      ["z", "order"],
      ["expiration", "1716453501"]
    ],
    "content": "",
    "sig": "7e8fe1eb644f33ff51d8805c02a0e1a6d034e6234eac50ef7a7e0dac68a0414f7910366204fa8217086f90eddaa37ded71e61f736d1838e37c0b73f6a16c4af2"
  }
]
```
