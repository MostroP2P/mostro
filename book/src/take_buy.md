# Taking a buy order

To take an order the seller will send to Mostro a message with the following content:

```json
{
  "order": {
    "version": 1,
    "pubkey": "00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "action": "take-buy",
    "content": null
  }
}
```

The event to send to Mostro would look like this:

```json
{
  "id": "cade205b849a872d74ba4d2a978135dbc05b4e5f483bb4403c42627dfd24f67d",
  "kind": 4,
  "pubkey": "1f5bb148a25bca31506594722e746b10acf2641a12725b12072dcbc46ade544d", // Seller's ephemeral pubkey
  "content": "base64-encoded-aes-256-cbc-encrypted-JSON-serialized-string",
  "tags": [
    ["p", "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a"] // Mostro's pubkey
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Mostro response

Mostro respond to the seller with a message with the following content:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "pay-invoice",
    "content": {
      "payment_request": [
        {
          "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
          "kind": "buy",
          "status": "waiting-payment",
          "amount": 7851,
          "fiat_code": "VES",
          "fiat_amount": 100,
          "payment_method": "face to face",
          "premium": 1,
          "created_at": 1698957793
        },
        "lnbcrt78510n1pj59wmepp50677g8tffdqa2p8882y0x6newny5vtz0hjuyngdwv226nanv4uzsdqqcqzzsxqyz5vqsp5skn973360gp4yhlpmefwvul5hs58lkkl3u3ujvt57elmp4zugp4q9qyyssqw4nzlr72w28k4waycf27qvgzc9sp79sqlw83j56txltz4va44j7jda23ydcujj9y5k6k0rn5ms84w8wmcmcyk5g3mhpqepf7envhdccp72nz6e"
      ]
    }
  }
}
```

Mostro updates the nip 33 event with `d` tag `ede61c96-4c13-4519-bf3a-dcf7f1e9d842` to change the status to `WaitingPayment`:

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
      ["s", "waiting-payment"],
      ["amt", "7851"],
      ["fa", "100"],
      ["pm", "face to face"],
      ["premium", "1"],
      ["y", "mostrop2p"],
      ["z", "order"]
    ],
    "content": "",
    "sig": "a835f8620db3ebdd9fa142ae99c599a61da86321c60f7c9fed0cc57169950f4121757ff64a5e998baccf6b68272aa51819c3e688d8ad586c0177b3cd1ab09c0f"
  }
]
```

And send a message to the buyer with the following content:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "waiting-seller-to-pay",
    "content": null
  }
}
```

## Seller pays LN invoice

After seller pays the hold invoice Mostro send a message to the seller with the following content:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "waiting-buyer-invoice",
    "content": null
  }
}
```

Mostro updates the nip 33 event with `d` tag `ede61c96-4c13-4519-bf3a-dcf7f1e9d842` to change the status to `waiting-buyer-invoice`:

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
      ["fa", "100"],
      ["pm", "face to face"],
      ["premium", "1"],
      ["y", "mostrop2p"],
      ["z", "order"]
    ],
    "content": "",
    "sig": "a835f8620db3ebdd9fa142ae99c599a61da86321c60f7c9fed0cc57169950f4121757ff64a5e998baccf6b68272aa51819c3e688d8ad586c0177b3cd1ab09c0f"
  }
]
```

And sends a message to the buyer with the following content:

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
        "status": "waiting-buyer-invoice",
        "amount": 7851,
        "fiat_code": "VES",
        "fiat_amount": 100,
        "payment_method": "face to face",
        "premium": 1,
        "created_at": null
      }
    }
  }
}
```

## Buyer sends LN invoice

Buyer sends the LN invoice to Mostro.

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "add-invoice",
    "content": {
      "payment_request": [
        null,
        "lnbcrt78510n1pj59wmepp50677g8tffdqa2p8882y0x6newny5vtz0hjuyngdwv226nanv4uzsdqqcqzzsxqyz5vqsp5skn973360gp4yhlpmefwvul5hs58lkkl3u3ujvt57elmp4zugp4q9qyyssqw4nzlr72w28k4waycf27qvgzc9sp79sqlw83j56txltz4va44j7jda23ydcujj9y5k6k0rn5ms84w8wmcmcyk5g3mhpqepf7envhdccp72nz6e"
      ]
    }
  }
}
```

Now both parties have an `active` order and they can keep going with the trade.

Finally Mostro updates the nip 33 event with `d` tag `ede61c96-4c13-4519-bf3a-dcf7f1e9d842` to change the status to `active`:

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
      ["s", "active"],
      ["amt", "7851"],
      ["fa", "100"],
      ["pm", "face to face"],
      ["premium", "1"],
      ["y", "mostrop2p"],
      ["z", "order"]
    ],
    "content": "",
    "sig": "a835f8620db3ebdd9fa142ae99c599a61da86321c60f7c9fed0cc57169950f4121757ff64a5e998baccf6b68272aa51819c3e688d8ad586c0177b3cd1ab09c0f"
  }
]
```
