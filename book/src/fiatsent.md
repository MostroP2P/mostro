# Fiat sent

After the buyer sends the fiat money to the seller, the buyer should send a message to Mostro indicating that the fiat money was sent, the message will look like this:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": "npub1qqqt938cer4dvlslg04zwwf66ts8r3txp6mv79cx2498pyuqx8uq0c7qkj",
  "action": "FiatSent",
  "content": null
}
```

## Mostro response

Mostro send a messages to both parties confirming `FiatSent` action and sending again the counterpart pubkey, here an example of the message to the buyer:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": "npub1qqqt938cer4dvlslg04zwwf66ts8r3txp6mv79cx2498pyuqx8uq0c7qkj",
  "action": "FiatSent",
  "content": {
    "Peer": {
      "pubkey": "npub1qqqxssz4k6swex94zdg5s4pqx3uqlhwsc2vdzvhjvzk33pcypkhqe9aeq2"
    }
  }
}
```

And here an example of the message to the seller:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": "npub1qqqxssz4k6swex94zdg5s4pqx3uqlhwsc2vdzvhjvzk33pcypkhqe9aeq2",
  "action": "FiatSent",
  "content": {
    "Peer": {
      "pubkey": "npub1qqqt938cer4dvlslg04zwwf66ts8r3txp6mv79cx2498pyuqx8uq0c7qkj"
    }
  }
}
```
