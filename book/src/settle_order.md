# Settle order

An admin can settle an order, most of the time this is done when admin is solving a dispute, for this the admin will need to send an `Order` message to Mostro with action `AdminSettle` with the `Id` of the order like this:

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

## Mostro response

Mostro will send this message to the both parties buyer/seller and to the admin:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "AdminSettle,",
    "content": null
  }
}
```

## Mostro send a nip 33 event to show the order is settled by the admin

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

Mostro updates the nip 33 order event with `d` tag `ede61c96-4c13-4519-bf3a-dcf7f1e9d842` to change the status to `SettledByAdmin`:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "316cf27758b0ae358dbc1fdcf27da38d80910543c8f74efeb77e7230910770ca",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1703274022,
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
    "sig": "27803a0f0f9961bff3ebdcd94c489dc27910002c51cbf297a0d95d8ced4f5d40d7b021ae342e90aa31acfdaee4536097368271b89239d9aed5de4dc986f6ed0b"
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

## Mostro tries to pay buyer's invoice

If the buyer's invoice is paid successfully Mostro will update the nip33 order event status `Success`:

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
      ["d", "b374ca1a-d596-419b-8d95-b8866044d892"],
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
