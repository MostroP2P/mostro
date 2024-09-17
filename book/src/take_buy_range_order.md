# Taking a buy range order

If the order fiat amount is a range like `10-20` the seller must indicate a fiat amount to take the order, seller will send a message in a Gift wrap Nostr event to Mostro with the following rumor's content:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "action": "take-buy",
    "content": {
      "amount": 15
    }
  }
}
```

## Mostro response

Response is the same as we explained in the [Taking a buy order](./take_buy.md) section.
