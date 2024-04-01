# Fiat sent

After the buyer sends the fiat money to the seller, the buyer should send a message to Mostro indicating that the fiat money was sent, the message will look like this:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427",
    "action": "fiat-sent",
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

Mostro send a messages to both parties confirming `fiat-sent` action and sending again the counterpart pubkey, here an example of the message to the buyer:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427",
    "action": "fiat-sent-ok",
    "content": {
      "Peer": {
        "pubkey": "00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78"
      }
    }
  }
}
```

And here an example of the message from Mostro to the seller:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78",
    "action": "fiat-sent-ok",
    "content": {
      "Peer": {
        "pubkey": "0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427"
      }
    }
  }
}
```

Mostro updates the nip 33 event with `d` tag `ede61c96-4c13-4519-bf3a-dcf7f1e9d842` to change the status to `fiat-sent`:

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
      ["s", "fiat-sent"],
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
