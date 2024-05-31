# Taking a sell order

If the order amount is `0` the buyer don't know the exact amount to create the invoice, buyer will send a message in a Nostr event kind 4 to Mostro with the following content:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427",
    "action": "take-sell",
    "content": null
  }
}
```

The event to send to Mostro would look like this:

```json
{
  "id": "cade205b849a872d74ba4d2a978135dbc05b4e5f483bb4403c42627dfd24f67d",
  "kind": 4,
  "pubkey": "9a42ac72d6466a6dbe5b4b07a8717ee13e55abb6bdd810ea9c321c9a32ee837b",
  "content": "base64-encoded-aes-256-cbc-encrypted-JSON-serialized-string",
  "tags": [
    ["p", "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a"]
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
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
        "fiat_amount": 100,
        "payment_method": "face to face",
        "premium": 1,
        "buyer_pubkey": null,
        "seller_pubkey": null
      }
    }
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

## Buyer sends LN invoice

The buyer sends a nip 04 event to Mostro with the lightning invoice, the action should be the same the buyer just received in the last message from Mostro (`add-invoice`), here the unencrypted content of the event for an invoice with no amount:

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
        "lnbcrt1pn9dvx0pp5935mskms2uf8wx90m8dlr60ytwn5vxy0e65ls42h7y7exweyvekqdqqcqzzsxqyz5vqsp5xjmllv4ta7jkuc5nfgqp8qjc3amzfewmlycpkkggr7q2y5mjfldq9qyyssqncpf3vm8hwujutqc99f0vy45zh8es54mn6u99q9t6rwm0q80dxszskzrp24y46lxqkc7ly9p80t6lalc8x8xhsn49yhy70a7wqyygugpv7chqs",
        3922
      ]
    }
  }
}
```

If the invoice includes an amount, the last element of the `payment_request` array should be set to `null`.

## Mostro response

Mostro send a nip 04 event to the buyer with a wrapped `order` in the content, it would look like this:

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
