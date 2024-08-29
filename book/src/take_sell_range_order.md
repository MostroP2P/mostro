# Taking a sell range order

If the order fiat amount is a range like `10-20` the buyer must indicate a fiat amount to take the order, buyer will send a message in a Nostr event kind 4 to Mostro with the following content:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427",
    "action": "take-sell",
    "content": {
      "amount": 15
    }
  }
}
```

## Mostro response

In order to continue the buyer needs to send a lightning network invoice to Mostro, in this case the amount of the order is `0`, so Mostro will need to calculate the amount of sats for this order, then Mostro will send back a message asking for a LN invoice indicating the correct amount of sats that the invoice should have, here the unencrypted content of the message:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "add-invoice",
    "content": {
      "order": {
        "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
        "amount": 7851,
        "fiat_code": "VES",
        "min_amount": 10,
        "max_amount": 20,
        "fiat_amount": 15,
        "payment_method": "face to face",
        "premium": 1,
        "master_buyer_pubkey": null,
        "master_seller_pubkey": null,
        "buyer_invoice": null,
        "created_at": null,
        "expires_at": null
      }
    }
  }
}
```

Mostro updates the parameterized replaceable event with `d` tag `ede61c96-4c13-4519-bf3a-dcf7f1e9d842` to change the status to `waiting-buyer-invoice`:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "eb0582360ebd3836c90711f774fbecb27e600f4a5fedf4fc2d16fc852f8380b1",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1702549437,
    "kind": 38383,
    "tags": [
      ["d", "ede61c96-4c13-4519-bf3a-dcf7f1e9d842"],
      ["k", "sell"],
      ["f", "VES"],
      ["s", "waiting-buyer-invoice"],
      ["amt", "7851"],
      ["fa", "15"],
      ["pm", "face to face"],
      ["premium", "1"],
      ["network", "mainnet"],
      ["layer", "lightning"],
      ["expiration", "1719391096"],
      ["y", "mostrop2p"],
      ["z", "order"]
    ],
    "content": "",
    "sig": "a835f8620db3ebdd9fa142ae99c599a61da86321c60f7c9fed0cc57169950f4121757ff64a5e998baccf6b68272aa51819c3e688d8ad586c0177b3cd1ab09c0f"
  }
]
```

## Using a lightning address

The buyer can use a [lightning address](https://github.com/andrerfneves/lightning-address) to receive funds and avoid to create and send lightning invoices on each trade, with a range order we set the fiat amount as the third element of the `payment_request` array, to acomplish this the buyer will send a message in a Nostr event kind 4 to Mostro with the following content:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427",
    "action": "take-sell",
    "content": {
      "payment_request": [null, "mostro_p2p@ln.tips", 15]
    }
  }
}
```
