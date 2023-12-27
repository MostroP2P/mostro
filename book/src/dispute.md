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
      ["y", "mostrop2p"],
      ["z", "dispute"]
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
    "id": "efc75871-2568-40b9-a6ee-c382d4d6de01",
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
        "status": "Active",
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
      ["y", "mostrop2p"],
      ["z", "dispute"]
    ],
    "content": "",
    "sig": "20d454a0704cfac1d4a6660d234ce407deb56db8f08598741af5d38c0698a96234fd326a34e7efb2ac20c1c0ed0a921fd50513aab8f5c4b83e2509f2d32794d2"
  }
]
```

## If admin settle the dispute

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "AdminSettle",
    "content": null
  }
}
```

Mostro will publish two nip33 messages, one for the order to update the status to `SettledByAdmin`, this means that the hold invoice paid by the seller was settled:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "8550d29d58ab9969e36c4d9f142df7b3672f091a5a30a24b069948f9a51a91af",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703683328,
    "kind": 38383,
    "tags": [
      ["d", "ede61c96-4c13-4519-bf3a-dcf7f1e9d842"],
      ["k", "Sell"],
      ["f", "VES"],
      ["s", "SettledByAdmin"],
      ["amt", "7851"],
      ["fa", "100"],
      ["pm", "face to face"],
      ["premium", "1"],
      ["y", "mostrop2p"],
      ["z", "order"]
    ],
    "content": "",
    "sig": "727d48b8ecb3883ed573ef047a2a6f023ab0e1c769dfeb29f6102e0ee308db6993c2b5b27953b903320ca92678fc50850dbe3238bfd89ad8732805c1f4491c6e"
  }
]
```

And another one for the dispute to update the status to `Settled`:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "4eb3f55e960d01cabfe085219a5f9cef22a78aaf2319de7b7096d8267d1ea32b",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703683328,
    "kind": 38383,
    "tags": [
      ["d", "efc75871-2568-40b9-a6ee-c382d4d6de01"],
      ["s", "Settled"],
      ["y", "mostrop2p"],
      ["z", "dispute"]
    ],
    "content": "",
    "sig": "7bc1b5c2f9bced642306605b07a454f68cf526609bbcbbbc3073bdeb7de493ec5c367a4d2d0e0d222517256fb11295cfae6b3229fa76e09ba368ded44ed1a3fe"
  }
]
```

## Payment of the buyer's invoice

At this point Mostro is trying to pay the buyer's invoice, right after complete the payment Mostro will update the status of the order nip33 event to `Success`:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "3ac1667178b728c40a1259d7ec82432b2a68f482d6b624637e95ca73f4778d4d",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703683338,
    "kind": 38383,
    "tags": [
      ["d", "ede61c96-4c13-4519-bf3a-dcf7f1e9d842"],
      ["k", "Sell"],
      ["f", "VES"],
      ["s", "Success"],
      ["amt", "7851"],
      ["fa", "100"],
      ["pm", "face to face"],
      ["premium", "1"],
      ["y", "mostrop2p"],
      ["z", "order"]
    ],
    "content": "",
    "sig": "4b509de7065f0ea16a6ef09063641f91e843c3bb3d0d0f2cf64616ebd6155ae251b3520e3b7ca25ececd84997966ea9df51c6f09877f19d43ad7e4f72e615afd"
  }
]
```

## If admin cancel the dispute

Here the `AdminCancel` message to Mostro:

```json
{
  "Orden": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "AdminCancel",
    "content": null
  }
}
```

Mostro will publish two nip33 messages, one for the order to update the status to `CanceledByAdmin`, this means that the hold invoice was canceled and the seller's funds were returned:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "886a2f7534106cdf64928ec214206ec313351acd28d3684a3076c7cb9c0bf759",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703684212,
    "kind": 38383,
    "tags": [
      ["d", "ede61c96-4c13-4519-bf3a-dcf7f1e9d842"],
      ["k", "Sell"],
      ["f", "VES"],
      ["s", "CanceledByAdmin"],
      ["amt", "7851"],
      ["fa", "100"],
      ["pm", "face to face"],
      ["premium", "1"],
      ["y", "mostrop2p"],
      ["z", "order"]
    ],
    "content": "",
    "sig": "ee6f0741e71864e683b822bae3c0d159f5ecae60d94d4160d18e18a02981f55ff5348d025618bb3c78a833fb07a72103116a140d64c20a90a50a88ddb772cfc3"
  }
]
```

And another one for the dispute to update the status to `SellerRefunded`:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "d05ed6edeb5dcbb37397f386b4e27fc0e0396af187b5d17fa7ffa921f01f460c",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703684212,
    "kind": 38383,
    "tags": [
      ["d", "efc75871-2568-40b9-a6ee-c382d4d6de01"],
      ["s", "SellerRefunded"],
      ["y", "mostrop2p"],
      ["z", "dispute"]
    ],
    "content": "",
    "sig": "00974f714294af68ce9f95e8c03d80f8e9fdcdb97ef24c71386fcafd0b2d4c680313008b9b8f8b3c231f1e0c2864db89e561f32cfdc9436817f97804d71c0a0c"
  }
]
```

Mostro will send this message to the both parties buyer/seller and to the admin:

```json
{
  "Dispute": {
    "version": "1",
    "id": "efc75871-2568-40b9-a6ee-c382d4d6de01",
    "pubkey": null,
    "action": "AdminCancel",
    "content": null
  }
}
```
