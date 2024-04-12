# User rating

After a successful trade Mostro send a nip04 event to both parties to let them know they can rate each other, here an example how the message look like:

```json
{
  "order": {
    "version": 1,
    "id": "7e44aa5d-855a-4b17-865e-8ca3834a91a3",
    "pubkey": null,
    "action": "rate",
    "content": null
  }
}
```

After a Mostro client receive this message, the user can rate the other party, the rating is a number between 1 and 5, to rate the client must receive user's input and create a new nip04 event to send to Mostro with this content:

```json
{
  "order": {
    "version": 1,
    "id": "7e44aa5d-855a-4b17-865e-8ca3834a91a3",
    "pubkey": null,
    "action": "rate-user",
    "content": {
      "rating_user": 5 // User input
    }
  }
}
```

## Confirmation message

If Mostro received the correct message, it will send back a confirmation message to the user with `Action: rate-received`:

```json
{
  "order": {
    "version": 1,
    "id": "7e44aa5d-855a-4b17-865e-8ca3834a91a3",
    "pubkey": null,
    "action": "rate-received",
    "content": null
  }
}
```

Mostro updates the nip 33 rating event, in this event the `d` tag will be the user pubkey `00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78` and looks like this:

```json
[
  "EVENT",
  "RAND",
  {
    "id": "80909a120d17632f99995f92caff4801f25e9e523d7643bf8acb0166bd0932a6",
    "pubkey": "dbe0b1be7aafd3cfba92d7463edbd4e33b2969f61bd554d37ac56f032e13355a",
    "created_at": 1702637077,
    "kind": 38383,
    "tags": [
      ["d", "00000ba40c5795451705bb9c165b3af93c846894d3062a9cd7fcba090eb3bf78"],
      ["total_reviews", "1"],
      ["total_rating", "2"],
      ["last_rating", "1"],
      ["max_rate", "2"],
      ["min_rate", "5"],
      ["data_label", "rating"]
    ],
    "content": "",
    "sig": "456fdc0589a5ffe1b55d5474cef2826bf01f458d63cf409490def9c5af31052e0461d38aed4f386f5dcea999e9fe6001d27d592dbba54a0420687dce0652322f"
  }
]
```
