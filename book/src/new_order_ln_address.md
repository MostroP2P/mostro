# Creating a new order

Creating buy order with a [lightning address](https://github.com/andrerfneves/lightning-address) would make the process way faster and easy going, to acomplish the buyer should send a Nostr event kind 4 (an encrypted message) to Mostro with the following content:

```json
{
  "Order": {
    "version": "1",
    "pubkey": "0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427", // Buyer's real pubkey
    "action": "NewOrder",
    "content": {
      "Order": {
        "kind": "Buy",
        "status": "Pending",
        "amount": 0,
        "fiat_code": "VES",
        "fiat_amount": 100,
        "payment_method": "face to face",
        "premium": 1,
        "buyer_invoice": "mostro_p2p@ln.tips",
        "created_at": 0
      }
    }
  }
}
```

The nostr event will look like this:

```json
{
  "id": "cade205b849a872d74ba4d2a978135dbc05b4e5f483bb4403c42627dfd24f67d",
  "kind": 4,
  "pubkey": "9a42ac72d6466a6dbe5b4b07a8717ee13e55abb6bdd810ea9c321c9a32ee837b", // Buyer's ephemeral pubkey
  "content": "base64-encoded-aes-256-cbc-encrypted-JSON-serialized-string",
  "tags": [
    ["p", "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a"] // Mostro's pubkey
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Confirmation message

Mostro will send back a nip04 event as a confirmation message to the user like the following:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427",
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
        "master_buyer_pubkey": null,
        "master_seller_pubkey": null,
        "buyer_invoice": "mostro_p2p@ln.tips",
        "created_at": 1698870173
      }
    }
  }
}
```

Mostro publishes this order as an event kind `38383` with status `Pending`:

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
      ["k", "Sell"],
      ["f", "VES"],
      ["s", "Pending"],
      ["amt", "0"],
      ["fa", "100"],
      ["pm", "face to face"],
      ["premium", "1"],
      ["y", "mostrop2p"],
      ["z", "order"]
    ],
    "content": "",
    "sig": "7e8fe1eb644f33ff51d8805c02a0e1a6d034e6234eac50ef7a7e0dac68a0414f7910366204fa8217086f90eddaa37ded71e61f736d1838e37c0b73f6a16c4af2"
  }
]
```

After a seller takes this order Mostro will not ask for an invoice to the buyer, Mostro will get the buyer's invoice and paid it when the seller releases the funds.
