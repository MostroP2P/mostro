# Taking a sell order

If the order amount is `0` the buyer don't know the exact amount to create the invoice, buyer will send a message in a Nostr event kind 4 to Mostro with the following content:

```json
{
  "version": "0",
  "pubkey": "npub1qqqt938cer4dvlslg04zwwf66ts8r3txp6mv79cx2498pyuqx8uq0c7qkj",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "action": "TakeSell",
  "content": null
}
```

## Mostro response

In order to continue the buyer needs to send a lightning network invoice to Mostro, in this case the amount of the order is `0`, so Mostro will need to calculate the amount of sats for this order, then Mostro will send back a message asking for a LN invoice indicating the correct amount of sats that the invoice should have:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "AddInvoice",
  "content": {
    "SmallOrder": {
      "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
      "amount": 7851,
      "fiat_code": "VES",
      "fiat_amount": 100,
      "payment_method": "face to face",
      "premium": 1,
      "buyer_pubkey": null,
      "seller_pubkey": null
    }
  }
}
```

## Buyer sends LN invoice

Here is how the buyer send the LN invoice to Mostro, in case the order has a fixed sats amount, the buyer can we skip the previous step:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "TakeSell",
  "content": {
    "PaymentRequest": [
      null,
      "lnbcrt32680n1pj59wmepp50677g8tffdqa2p8882y0x6newny5vtz0hjuyngdwv226nanv4uzsdqqcqzzsxqyz5vqsp5skn973360gp4yhlpmefwvul5hs58lkkl3u3ujvt57elmp4zugp4q9qyyssqw4nzlr72w28k4waycf27qvgzc9sp79sqlw83j56txltz4va44j7jda23ydcujj9y5k6k0rn5ms84w8wmcmcyk5g3mhpqepf7envhdccp72nz6e"
    ]
  }
}
```

## Mostro response

Mostro send the following message to the buyer:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "WaitingSellerToPay",
  "content": null
}
```
