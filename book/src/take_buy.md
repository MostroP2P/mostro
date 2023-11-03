# Taking a buy order

To take an order the seller will send to Mostro a message with the following content:

```json
{
  "version": "0",
  "pubkey": "npub1qqqxssz4k6swex94zdg5s4pqx3uqlhwsc2vdzvhjvzk33pcypkhqe9aeq2",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "action": "TakeBuy",
  "content": {
    "Peer": {
      "pubkey": "npub1qqqxssz4k6swex94zdg5s4pqx3uqlhwsc2vdzvhjvzk33pcypkhqe9aeq2"
    }
  }
}
```

## Mostro response

Mostro respond to the seller with a message with the following content:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "PayInvoice",
  "content": {
    "PaymentRequest": [
      {
        "id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
        "kind": "Buy",
        "status": "WaitingPayment",
        "amount": 7851,
        "fiat_code": "VES",
        "fiat_amount": 100,
        "payment_method": "face to face",
        "premium": 1,
        "created_at": 1698957793
      },
      "lnbcrt32680n1pj59wmepp50677g8tffdqa2p8882y0x6newny5vtz0hjuyngdwv226nanv4uzsdqqcqzzsxqyz5vqsp5skn973360gp4yhlpmefwvul5hs58lkkl3u3ujvt57elmp4zugp4q9qyyssqw4nzlr72w28k4waycf27qvgzc9sp79sqlw83j56txltz4va44j7jda23ydcujj9y5k6k0rn5ms84w8wmcmcyk5g3mhpqepf7envhdccp72nz6e"
    ]
  }
}
```

And send a message to the buyer with the following content:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "WaitingSellerToPay",
  "content": null
}
```

## Seller pays LN invoice

After seller pays the hold invoice Mostro send a message to the seller with the following content:

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "WaitingBuyerInvoice",
  "content": null
}
```

And sends a message to the buyer with the following content:

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

Buyer sends the LN invoice to Mostro.

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "AddInvoice",
  "content": {
    "PaymentRequest": [
      null,
      "lnbcrt32680n1pj59wmepp50677g8tffdqa2p8882y0x6newny5vtz0hjuyngdwv226nanv4uzsdqqcqzzsxqyz5vqsp5skn973360gp4yhlpmefwvul5hs58lkkl3u3ujvt57elmp4zugp4q9qyyssqw4nzlr72w28k4waycf27qvgzc9sp79sqlw83j56txltz4va44j7jda23ydcujj9y5k6k0rn5ms84w8wmcmcyk5g3mhpqepf7envhdccp72nz6e"
    ]
  }
}
```
