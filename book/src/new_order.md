# Creating a new order

To create a new sell order the user should send a Nostr event kind 4 to Mostro with the following content:

```json
{
  "Order": {
    "version": "1",
    "pubkey": "npub1qqq...",
    "action": "NewOrder",
    "content": {
      "Order": {
        "kind": "Sell",
        "status": "Pending",
        "amount": 0,
        "fiat_code": "VES",
        "fiat_amount": 100,
        "payment_method": "face to face",
        "premium": 1,
        "created_at": 0
      }
    }
  }
}
```

Let's explain some of the fields:

- kind: `Sell` or `Buy`
- status: Is always `Pending` when creating a new order
- amount: 0 for when we want to sell with at market price, otherwise the amount in satoshis
- pubkey: Real user's npub, we use this when the message was sent from an ephemeral key
- created_at: No need to send the correct unix timestamp, Mostro will replace it with the current time

## Confirmation message

Mostro will send back a confirmation message to the user like the following:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "npub1qqq...",
    "action": "NewOrder",
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
        "created_at": 1698870173
      }
    }
  }
}
```
