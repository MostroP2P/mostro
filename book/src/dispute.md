# Dispute

A use can start a dispute in an order with status `Pending` or `FiatSent` sending action `Dispute`, here is an example where the seller initiates a dispute:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78",
    "action": "Dispute",
    "content": null
  }
}
```

## Mostro response

Mostro will send this message to the seller:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "DisputeInitiatedByYou,",
    "content": null
  }
}
```

And here is the message to the buyer:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "DisputeInitiatedByPeer",
    "content": null
  }
}
```

Mostro will not update the nip 33 event with `d` tag `ede61c96-4c13-4519-bf3a-dcf7f1e9d842` to change the status to `Dispute`, this is because the order is still active, the dispute is just a way to let the admins and the other party know that there is a problem with the order.

## Mostro send a nip 33 event to show the dispute

Here is an example of the nip 33 event sent by Mostro:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "4a4d63698f8a27d7d44e5669224acf6af2516a9350ae5f07d3cb91e5601f7302",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703016565,
    "kind": 38383,
    "tags": [
      ["d", "efc75871-2568-40b9-a6ee-c382d4d6de01"],
      ["s", "Pending"],
      ["data_label", "dispute"]
    ],
    "content": "",
    "sig": "00a1da45c00684c5af18cf292ca11697c9e70f2a691e6cd397211e717d2f54362dd401d7567da8184a5c596f48a09693479e67214c23e773523a63d0b1c3f537"
  }
]
```

Mostro admin will see the dispute and can take it using the dispute `Id` from `d` tag, in this case `efc75871-2568-40b9-a6ee-c382d4d6de01`.

```json
{
  "Dispute": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "AdminTakeDispute",
    "content": null
  }
}
```

Mostro will send a confirmation message to the admin with the Order details:

```json
{
  "Dispute": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "AdminTakeDispute",
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
        "master_buyer_pubkey": "0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427",
        "master_seller_pubkey": "00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78",
        "buyer_invoice": "lnbcrt11020n1pjcypj3pp58m3d9gcu4cc8l3jgkpfn7zhqv2jfw7p3t6z3tq2nmk9cjqam2c3sdqqcqzzsxqyz5vqsp5mew44wzjs0a58d9sfpkrdpyrytswna6gftlfrv8xghkc6fexu6sq9qyyssqnwfkqdxm66lxjv8z68ysaf0fmm50ztvv773jzuyf8a5tat3lnhks6468ngpv3lk5m7yr7vsg97jh6artva5qhd95vafqhxupyuawmrcqnthl9y",
        "created_at": 1698870173
      }
    }
  }
}
```

Also Mostro will broadcast a new nip33 dispute event to update the Dispute `status` to `InProgress`:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "2bb3f5a045bcc1eb057fd1e22c0cece7c58428a6ab5153299ef4e1e89633fde9",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703020540,
    "kind": 38383,
    "tags": [
      ["d", "efc75871-2568-40b9-a6ee-c382d4d6de01"],
      ["s", "InProgress"],
      ["data_label", "dispute"]
    ],
    "content": "",
    "sig": "20d454a0704cfac1d4a6660d234ce407deb56db8f08598741af5d38c0698a96234fd326a34e7efb2ac20c1c0ed0a921fd50513aab8f5c4b83e2509f2d32794d2"
  }
]
```
