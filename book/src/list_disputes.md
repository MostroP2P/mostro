# Listing Disputes

Mostro publishes new disputes with event kind `38383` and status `initiated`:

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
      ["s", "initiated"],
      ["y", "mostrop2p"],
      ["z", "dispute"]
    ],
    "content": "",
    "sig": "00a1da45c00684c5af18cf292ca11697c9e70f2a691e6cd397211e717d2f54362dd401d7567da8184a5c596f48a09693479e67214c23e773523a63d0b1c3f537"
  }
]

Clients can query this events by nostr event kind `38383`, nostr event author, dispute status (`s`), type (`z`)
```
