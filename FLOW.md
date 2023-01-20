# Mostro

This document explains how works the `order flow` on Mostro, this is work in progress.

## Overview

All mostro messages are [Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/33.md) and use `30000` as event `kind`, a list of standard event kinds can be found [here](https://github.com/nostr-protocol/nips)

## Keys

For this example the participants will use this keys:

- Mostro's pubkey `7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8`
- Seller's pubkey `1f5bb148a25bca31506594722e746b10acf2641a12725b12072dcbc46ade544d`
- Buyer's pubkey `f6c63403def1642b0980c42221f1649cdc33d01ce4156c93f6e1607f3e854c92`

## Communication between users and Mostro

All messages to Mostro should be a Nostr event kind 4, and should have this fields:

- `version`
- `action` (Order/TakeSell/TakeBuy/PayInvoice/FiatSent/Release)
- `content` (optional to be used on action `Order` or `PaymentRequest`)

Example of a message from a buyer sending a lightning network invoice, the content of this message should be a JSON-serialized string (with no white space or line breaks) of the following structure:

```json
{
  "version": "0",
  "order_id": 54,
  "action": "TakeSell",
  "content": {
    "PaymentRequest": "lnbcrt500u1p3e0xwkpp585pza8m5klgy3zn4dw7ej32jh0hz5mrucc04aezcjx2uulr4tf2sdqqcqzpgxqyz5vqsp52m65dwsqkq5n630pareeswal9e2xxx0ldykuhhcfc0ed2znwzmfq9qyyssqz422f9qtwcleykknzq29yhyytufddhnml4hqdtu3mtpw37kvltqkp7z4y6ntkhy7vpy2eyy53qzjsa0u7mmmx8ee5td64c8x4vm2vcsq786ewz"
  }
}
```

## Order

To publish an order a user needs to send an encrypted message to Mostro with the order details, then Mostro will create a new `Parameterized Replaceable Event` that could be taken by another user that wants to trade.

The order wrapped on the encrypted message have this properties:

- `kind` (Buy/Sell)
- `status` (this will be handle by Mostro, user should send Pending to publish)
- `amount`
- `fiat_code`
- `fiat_amount`
- `payment_method`
- `prime`
- `payment_request` (optional, to be used only on Buy orders)

This format is subject to change!

Example of message from a buyer to create a buy order:

```json
{
  "version": "0",
  "action": "Order",
  "content": {
    "Order": {
      "kind": "Buy",
      "amount": 6000,
      "fiat_code": "EUR",
      "fiat_amount": 1,
      "payment_method": "bank transfer",
      "prime": 0,
      "payment_request": "lnbcrt500u1p3e0xwkpp585pza8m5klgy3zn4dw7ej32jh0hz5mrucc04aezcjx2uulr4tf2sdqqcqzpgxqyz5vqsp52m65dwsqkq5n630pareeswal9e2xxx0ldykuhhcfc0ed2znwzmfq9qyyssqz422f9qtwcleykknzq29yhyytufddhnml4hqdtu3mtpw37kvltqkp7z4y6ntkhy7vpy2eyy53qzjsa0u7mmmx8ee5td64c8x4vm2vcsq786ewz"
    }
  }
}
```

## Seller creates an order

The seller wants to exchange `100` sats and get `1000` of `XXX` currency, to publish an offer the seller send an [encrypted event](https://github.com/nostr-protocol/nips/blob/master/04.md) to Mostro's pubkey, the content of this event should be a JSON-serialized string (with no white space or line breaks) of the following structure:

```json
{
  "version": "0",
  "action": "Order",
  "content": {
    "Order": {
      "kind": "Sell",
      "status": "Pending",
      "amount": 100,
      "fiat_code": "XXX",
      "fiat_amount": 1000,
      "payment_method": "bank transfer",
      "prime": 1
    }
  }
}
```

Event example:

```json
{
  "id": "cade205b849a872d74ba4d2a978135dbc05b4e5f483bb4403c42627dfd24f67d",
  "kind": 4,
  "pubkey": "1f5bb148a25bca31506594722e746b10acf2641a12725b12072dcbc46ade544d",
  "content": "base64encoded-encrypted-order",
  "tags": [
    ["p", "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8"]
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

Mostro publishes this order as an event kind `30000` with status `Pending`:

```json
{
  "id": "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad",
  "kind": 30000,
  "pubkey": "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8",
  "content": "{\"version\":0,\"order_id\":54,\"kind\":\"Sell\",\"status\":\"Pending\",\"amount\":100,\"fiat_code\":\"XXX\",\"fiat_amount\":1000,\"payment_method\":\"bank transfer\",\"prime\":1,\"payment_request\":null}",
  "tags": [],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Buyer takes an order

The buyer wants to buy sats and take the order:

### Buyer send a regular invoice

Buyer sends an encrypted message to mostro's pubkey with a lightning invoice, this invoice can have an amount of 100 sats or be amountless, this is linked to :

Unencrypted content:

```json
{
  "version": "0",
  "order_id": 54,
  "action": "TakeSell",
  "content": {
    "PaymentRequest": "lnbcrt1u1p3e0geapp5u3nfpcmc4llggqq6upp85p32kvph6uh8caqkruph5xh0lgl4764qdqqcqzpgxqyz5vqsp59ul6delmlj35rk0k5hcfxz9q0xfcgdsflkzpf673g08dhkm6gtjq9qyyssqe6daccezwpjxxm7n7nqh3zw5ykjl42wmneaukhedaz037t0tarmjnfay3j3xddwz6eg7q98zxct32trfq3h2tr72xyhrkls255q4wfspn84a2e"
  }
}
```

Nostr event:

```json
{
  "id": "8af95e0ae6dcf65505474ea8885b3f2eb46c1f094f06339f76c711af43a2242d",
  "kind": 4,
  "pubkey": "f6c63403def1642b0980c42221f1649cdc33d01ce4156c93f6e1607f3e854c92",
  "content": "base64encoded-encrypted-invoice",
  "tags": [
    ["p", "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8"]
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Mostro put them in touch

Mostro sends an encrypted event to seller with a hold invoice:

Unencrypted message from Mostro to user:

```json
{
  "version": "0",
  "action": "PayInvoice",
  "content": {
    "PaymentRequest": "lnbcrt1u1p3e0geapp5u3nfpcmc4llggqq6upp85p32kvph6uh8caqkruph5xh0lgl4764qdqqcqzpgxqyz5vqsp59ul6delmlj35rk0k5hcfxz9q0xfcgdsflkzpf673g08dhkm6gtjq9qyyssqe6daccezwpjxxm7n7nqh3zw5ykjl42wmneaukhedaz037t0tarmjnfay3j3xddwz6eg7q98zxct32trfq3h2tr72xyhrkls255q4wfspn84a2e"
  }
}
```

After the seller pays the invoice mostro put the parties in touch and update the order sending a replaceable event kind `30000` with the same id, a newer timestamp and status `Active`:

```json
{
  "id": "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad",
  "kind": 30000,
  "pubkey": "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8",
  "content": "{\"version\":0,\"order_id\":54,\"kind\":\"Sell\",\"status\":\"Active\",\"amount\":100,\"fiat_code\":\"XXX\",\"fiat_amount\":1000,\"payment_method\":\"bank transfer\",\"prime\":1,\"payment_request\":null}",
  "tags": [],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Mostro talks to seller

The buyer sends the seller fiat money, after that, the buyer sends an encrypted message to Mostro indicating that the fiat was sent, example:

Unencrypted `fiat sent` message:

```json
{
  "version": "0",
  "order_id": 54,
  "action": "FiatSent"
}
```

Encrypted content event example:

```json
{
  "id": "581c0f6f7f8561737506d4484e0e28e18852d8543a9bbcea34ff0dfe68961046",
  "kind": 4,
  "pubkey": "f6c63403def1642b0980c42221f1649cdc33d01ce4156c93f6e1607f3e854c92",
  "content": "base64encoded-encrypted-fiatsent",
  "tags": [
    ["p", "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8"]
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

Now Mostro send a replaceable event kind `30000` with the same id, a newer timestamp and status `FiatSent`:

```json
{
  "id": "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad",
  "kind": 30000,
  "pubkey": "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8",
  "content": "{\"version\":0,\"order_id\":54,\"kind\":\"Sell\",\"status\":\"FiatSent\",\"amount\":100,\"fiat_code\":\"XXX\",\"fiat_amount\":1000,\"payment_method\":\"bank transfer\",\"prime\":1,\"payment_request\":null}",
  "tags": [],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Mostro request release of funds

Mostro send an encrypted message to seller indicating that buyer confirmed that fiat was sent and request to release funds, if everything went well, seller respond with a new encrypted message to Mostro with this content to release funds:

Unencrypted `release` message:

```json
{
  "version": "0",
  "order_id": 54,
  "action": "Release"
}
```

## Settle seller's invoice

Mostro settle the invoice and send a replaceable event kind `30000` with the same id, a newer timestamp and status `SettledHoldInvoice`, right after tries to pay the buyer's invoice, after the invoice is paid Mostro send a replaceable event kind `30000` with status `Success`.
