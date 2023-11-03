# Release

After confirming the buyer sent the fiat money, the seller should send a message to Mostro indicating that sats should be delivered to the buyer, the message will look like this:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": "npub1qqqxssz4k6swex94zdg5s4pqx3uqlhwsc2vdzvhjvzk33pcypkhqe9aeq2",
  "action": "Release",
  "content": null
}
```

## Mostro response

Here an example of the Mostro response to the seller:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "HoldInvoicePaymentSettled",
  "content": null
}
```

And a message to the buyer to let him know that the sats were released:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "Release",
  "content": null
}
```

## Buyer receives sats

Right after seller release sats Mostro will try to pay the buyer's lightning invoice, if the payment is successful Mostro will send a message to the buyer indicating that the purchase was completed:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "PurchaseCompleted",
  "content": null
}
```
