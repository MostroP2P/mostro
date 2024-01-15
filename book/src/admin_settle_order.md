# Settle order

An admin can settle an order, most of the time this is done when admin is solving a dispute, for this the admin will need to send an `Order` message to Mostro with action `AdminSettle` with the `Id` of the order like this:

```json
{
  "Order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "AdminSettle",
    "content": null
  }
}
```

## Mostro response

Mostro will send this message to the both parties buyer/seller and to the admin:

```json
{
  "Order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "AdminSettle",
    "content": null
  }
}
```

## Mostro updates nip 33 events

Mostro will publish two nip33 messages, one for the order to update the status to `SettledByAdmin`, this means that the hold invoice paid by the seller was settled:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "3d74ce3f10096d163603aa82beb5778bd1686226fdfcfba5d4c3a2c3137929ea",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703260182,
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
    "sig": "76bfc5e9ce089757dd4074472e1df421da700ce133c874f40b1136607121eca8acfdd2b8b4b374adaa83fa0c7d99672eb21a1068b6b6b774742d5de5bfc932ba"
  }
]
```

And updates nip33 dispute event with status `Settled`:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "098e8622eae022a79bc793984fccbc5ea3f6641bdcdffaa031c00d3bd33ca5a0",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703274022,
    "kind": 38383,
    "tags": [
      ["d", "efc75871-2568-40b9-a6ee-c382d4d6de01"],
      ["s", "Settled"],
      ["y", "mostrop2p"],
      ["z", "dispute"]
    ],
    "content": "",
    "sig": "6d7ca7bef7b696f1f6f8cfc33b3fe1beb2fdc6b7647efc93be669c6c1a9d4bafc770d9b0d25432c204dd487d48b39e589dfd7b03bf0e808483921b8937bd5367"
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
    "id": "6170892aca6a73906142e58a9c29734d49b399a3811f6216ce553b4a77a8a11e",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703274032,
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
    "sig": "1670a9e61f7bc99f7121a95a2d479456970fbd9bc84d663160e35d1a95d71a006c7986db050ea584d5040927879fd9dcc85dc0ab5c6367f679c9fd5fd33a3cfb"
  }
]
```
