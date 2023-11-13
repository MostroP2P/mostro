# User rating

After a successful trade Mostro send a nip04 event to both parties to let them know they can rate each other, here an example how the message look like:

```json
{
  "version": "0",
  "order_id": "7e44aa5d-855a-4b17-865e-8ca3834a91a3",
  "pubkey": null,
  "action": "RateUser",
  "content": null
}
```

After a Mostro client receive this message, the user can rate the other party, the rating is a number between 1 and 5, to rate the client must receive user's input and create a new nip04 event to send to Mostro with this content:

```json
{
  "version": "0",
  "order_id": "7e44aa5d-855a-4b17-865e-8ca3834a91a3",
  "pubkey": null,
  "action": "RateUser",
  "content": {
    "RatingUser": 5 // User input
  }
}
```

## Confirmation message

If Mostro received the message correct it will send back a last confirmation message to the user with `Action: Received`:

```json
{
  "version": "0",
  "order_id": "7e44aa5d-855a-4b17-865e-8ca3834a91a3",
  "pubkey": null,
  "action": "Received",
  "content": null
}
```
