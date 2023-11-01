# Take a sell order

## Overview

All Mostro messages are [Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/01.md#kinds) and use `30078` as event `kind`, a list of standard event kinds can be found [here](https://github.com/nostr-protocol/nips#event-kinds)

## Communication between users and Mostro

All messages from/to Mostro should be a Nostr event kind 4, the content of the event should be a JSON-serialized string (with no white space or line breaks) of the following structure:

- `version`
- `order_id` (optional)
- `pubkey` (optional)
- `action` (https://docs.rs/mostro-core/latest/mostro_core/enum.Action.html)
- `content` (optional https://docs.rs/mostro-core/latest/mostro_core/enum.Content.html)

## Taking a sell order

To take a new sell order the user should send a Nostr event kind 4 to Mostro with the following content:

```json
{
  "version": "0",
  "pubkey": "npub1qqq...",
  "order_id": "68e373ef-898b-4312-9f49-dfc50404e3b2",
  "action": "TakeSell",
  "content": null
}
```

## Mostro response

In order to continue the buyer needs to send a lightning network invoice to Mostro, if the amount of the order is `0`, Mostro will need to calculate the amount of sats of this order, then Mostro will send back a message asking for a LN invoice indicating the correct amount of sats that the invoice should have:

```json
{
  "version": "0",
  "order_id": "68e373ef-898b-4312-9f49-dfc50404e3b2",
  "pubkey": null,
  "action": "AddInvoice",
  "content": {
    "SmallOrder": {
      "id": "68e373ef-898b-4312-9f49-dfc50404e3b2",
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

```json
{
  "version": "0",
  "order_id": "ede61c96-4c13-4519-bf3a-dcf7f1e9d842",
  "pubkey": null,
  "action": "WaitingSellerToPay",
  "content": null
}
```
