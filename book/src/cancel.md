# Cancel Order

A use can cancel an Order created by himself and with status `Pending` sending action `Cancel`, the message will look like this:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": "npub1qqqt938cer4dvlslg04zwwf66ts8r3txp6mv79cx2498pyuqx8uq0c7qkj",
  "action": "Cancel",
  "content": null
}
```

## Mostro response

Mostro will send a message with action `Cancel` confirming the order was canceled, here an example of the message:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "Cancel",
  "content": null
}
```

## Cancel cooperatively

A user can cancel an `Active` order, but will need the counterparty to agree, let's look at an example where the seller initiates a cooperative cancellation:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "Cancel",
  "content": null
}
```

Mostro will send this message to the seller:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "CooperativeCancelInitiatedByYou",
  "content": null
}
```

And this message to the buyer:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "CooperativeCancelInitiatedByPeer",
  "content": null
}
```

The buyer can accept the cooperative cancellation sending this message:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "Cancel",
  "content": null
}
```
