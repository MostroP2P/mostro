# Creating a new buy order

To create a new buy order the user should send a Gift wrap Nostr event to Mostro with the following rumor's content:

```json
{
  "order": {
    "version": 1,
    "action": "new-order",
    "content": {
      "order": {
        "kind": "buy",
        "status": "pending",
        "amount": 0,
        "fiat_code": "VES",
        "fiat_amount": 100,
        "payment_method": "face to face",
        "premium": 1,
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
  "kind": 1059,
  "pubkey": "9a42ac72d6466a6dbe5b4b07a8717ee13e55abb6bdd810ea9c321c9a32ee837b", // Buyer's ephemeral pubkey
  "content": "sealed-rumor-content",
  "tags": [
    ["p", "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a"] // Mostro's pubkey
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Confirmation message

Mostro will send back a nip59 event as a confirmation message to the user like the following:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "content": {
      "order": {
        "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
        "kind": "buy",
        "status": "pending",
        "amount": 0,
        "fiat_code": "VES",
        "fiat_amount": 100,
        "payment_method": "face to face",
        "premium": 1,
        "master_buyer_pubkey": null,
        "master_seller_pubkey": null,
        "buyer_invoice": null,
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
      ["k", "buy"],
      ["f", "VES"],
      ["s", "pending"],
      ["amt", "0"],
      ["fa", "100"],
      ["pm", "face to face"],
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

After a seller takes this order Mostro will request an invoice from the buyer, Mostro will pay the buyer's invoice when the seller releases the funds.

## Creating the order with a lightning invoice

There are two ways where the buyer can create the order adding the invoice:

1. If the buyer already knows the amount, e.g. buyer wants to buy 15000 sats with 10 euros, in this case the buyer knows from the beginning that the invoice should have amount 15000 - Mostro's fee, instead of the user doing the calculation, the client must do it and in some cases create the invoice with the right amount.

2. If the buyer don't know the amount, e.g. buyer wants to buy sats with 10 euros, in this case the buyer can add an amountless invoice and Mostro will pay it with the market price amount - Mostro's fee automatically.
