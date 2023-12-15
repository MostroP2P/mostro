# Dispute

A use can start a dispute in an order with status `Pending` or `FiatSent` sending action `Dispute`, here is an example where the seller initiates a dispute:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": "npub1qqqt938cer4dvlslg04zwwf66ts8r3txp6mv79cx2498pyuqx8uq0c7qkj",
    "action": "Dispute",
    "content": null
  }
}
```

## Mostro response

Mostro will send this message to the seller:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "DisputeInitiatedByYou,",
    "content": null
  }
}
```

And here is the message to the buyer:

```json
{
  "Order": {
    "version": "1",
    "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
    "pubkey": null,
    "action": "DisputeInitiatedByPeer",
    "content": null
  }
}
```

Mostro will not update the nip 33 event with `d` tag `ede61c96-4c13-4519-bf3a-dcf7f1e9d842` to change the status to `Dispute`, this is because the order is still active, the dispute is just a way to let the admins and the other party know that there is a problem with the order.
