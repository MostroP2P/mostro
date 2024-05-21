# Taking a buy range order

If the order fiat amount is a range like `10-20` the seller must indicate a fiat amount to take the order, seller will send a message in a Nostr event kind 4 to Mostro with the following content:

```json
{
  "order": {
    "version": 1,
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "0000147e939bef2b81c27af4c1b702c90c3843f7212a34934bff1e049b7f1427",
    "action": "take-buy",
    "content": {
      "amount": 15
    }
  }
}
```

## Mostro response

Response is the same as we explained in the [Taking a buy order](./take_buy.md) section.
