# Listing Orders

Mostro publishes new orders with event kind `38383` and status `pending`:

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
      ["k", "sell"],
      ["f", "VES"],
      ["s", "pending"],
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

Clients can query this events by nostr event kind `38383`, nostr event author, order status (`s`), order kind (`k`), order currency (`f`), type (`z`)
