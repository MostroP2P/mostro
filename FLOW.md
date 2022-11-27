# Mostro

This document explains how works the `order flow` on Mostro, this is work in progress.

## Overview

All mostro messages are replaceable events and use `11000` as event `kind`, a list of standard event kinds can be found [here](https://github.com/nostr-protocol/nips)

## Keys

For this example the participants will use this keys:

- Mostro's pubkey `7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8`
- Seller's pubkey `1f5bb148a25bca31506594722e746b10acf2641a12725b12072dcbc46ade544d`
- Buyer's pubkey `f6c63403def1642b0980c42221f1649cdc33d01ce4156c93f6e1607f3e854c92`

## Order event

To publish an order a user needs to send an encrypted message to Mostro with the order details, then Mostro will create a new `replaceable event` that could be taken by another user that wants to trade.

Contents of an order event have this properties:

- `version`
- `kind` (buy/sell)
- `status` (this will be handle by Mostro)
- `amount`
- `fiat_code`
- `fiat_amount`
- `payment_method`
- `prime`
- `invoice` (optional)

This format is subject to change!

Example:

```json
{
  "version": "0",
  "kind": "sell",
  "status": "PENDING",
  "amount": 100,
  "fiat_code": "XXX",
  "fiat_amount": 1000,
  "payment_method": "bank transfer",
  "prime": 0,
  "invoice": "lnbc1..."
}
```

## Seller creates an order

The seller wants to exchange `100` sats and get `1000` of `XXX` currency, to publish an offer the seller send an [encrypted event](https://github.com/nostr-protocol/nips/blob/master/04.md) to Mostro's pubkey, the content of this event should be a JSON-serialized string (with no white space or line breaks) of the following structure:

```json
{
  "version": "0",
  "kind": "sell",
  "amount": 100,
  "fiat_code": "XXX",
  "fiat_amount": 1000,
  "payment_method": "bank transfer",
  "prime": 0
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

Mostro publishes this order as an event kind `11000` with status `PENDING`:

```json
{
  "id": "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad",
  "kind": 11000,
  "pubkey": "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8",
  "content": "{\"version\":0,\"kind\":\"sell\",\"status\":\"PENDING\",\"amount\":100,\"fiat_code\":\"XXX\",\"fiat_amount\":1000,\"payment_method\":\"bank transfer\",\"prime\":0}",
  "tags": [],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Buyer takes an order

The buyer wants to buy sats and take the order, for this the buyer have two options:

### 1. Buyer own hold invoice

_This is theoretical and probably not feasible, for this option to work, the buyer needs to have a wallet or node that allows to create hold invoices making the process slow and probably and difficult for newbies._

Send an encrypted event to Mostro requesting a payment hash to create a hold invoice

content:

```json
{
  "request": "payment_hash"
}
```

```json
{
  "id": "fd64c40785e7de94b726ed214788d9778d2167167fe5c1d87d8baca581e6a91e",
  "kind": 4,
  "pubkey": "f6c63403def1642b0980c42221f1649cdc33d01ce4156c93f6e1607f3e854c92",
  "content": "base64encoded-encrypted-request",
  "tags": [
    ["p", "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8"],
    ["e", "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad"]
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

Mostro creates a secret, hash it and send it to the buyer in an encrypted event.

```json
{
  "id": "e31c46790d8ba9606feb9311bb457b4ed2f521ac3a3bf5fd9a0c4a125f49d0df",
  "kind": 4,
  "pubkey": "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8",
  "content": "base64encoded-encrypted-hash",
  "tags": [
    ["p", "f6c63403def1642b0980c42221f1649cdc33d01ce4156c93f6e1607f3e854c92"],
    ["e", "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad"]
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

Now the buyer can create a hold invoice without knowing the secret, just with the hash, the new invoice can be created without amount or with the exact amount of 100 sats, then the buyer encrypt, encode the invoice and then send it to Mostro in a new event:

```json
{
  "id": "8af95e0ae6dcf65505474ea8885b3f2eb46c1f094f06339f76c711af43a2242d",
  "kind": 4,
  "pubkey": "f6c63403def1642b0980c42221f1649cdc33d01ce4156c93f6e1607f3e854c92",
  "content": "base64encoded-encrypted-invoice",
  "tags": [
    ["p", "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8"],
    ["e", "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad"]
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

### 2. Buyer send a regular invoice

Buyer sends an encrypted message to mostro's pubkey with a lightning invoice, this invoice can have an amount of 100 sats or be amountless:

```json
{
  "id": "8af95e0ae6dcf65505474ea8885b3f2eb46c1f094f06339f76c711af43a2242d",
  "kind": 4,
  "pubkey": "f6c63403def1642b0980c42221f1649cdc33d01ce4156c93f6e1607f3e854c92",
  "content": "base64encoded-encrypted-invoice",
  "tags": [
    ["e", "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad"],
    ["p", "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8"]
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Mostro put them in contact

Mostro sends an encrypted event to seller with a hold invoice, after the seller pays the invoice mostro put the parties in contact and update the order sending a replaceable event kind `11000` with the same id, a newer timestamp and status `ACTIVE`:

```json
{
  "id": "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad",
  "kind": 11000,
  "pubkey": "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8",
  "content": "{\"version\":0,\"kind\":\"sell\",\"status\":\"ACTIVE\",\"amount\":100,\"fiat_code\":\"XXX\",\"fiat_amount\":1000,\"payment_method\":\"bank transfer\",\"prime\":0}",
  "tags": [],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Mostro talks to seller

The buyer sends the seller fiat money, after that, the buyer sends an encrypted message to Mostro indicating that the fiat was sent, example:

```json
{
  "fiat_sent": true
}
```

Event example:

```json
{
  "id": "581c0f6f7f8561737506d4484e0e28e18852d8543a9bbcea34ff0dfe68961046",
  "kind": 4,
  "pubkey": "f6c63403def1642b0980c42221f1649cdc33d01ce4156c93f6e1607f3e854c92",
  "content": "base64encoded-encrypted-fiatsent",
  "tags": [
    ["p", "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8"],
    ["e", "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad"]
  ],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

Now Mostro send a replaceable event kind `11000` with the same id, a newer timestamp and status `FIAT_SENT`:

```json
{
  "id": "74a1ce6e428ba3b4d7c99a5f582b04afdb645aa5f0c661cf83ed3c4e547c04ad",
  "kind": 11000,
  "pubkey": "7590450f6b4d2c6793cacc8c0894e2c6bd2e8a83894912e79335f8f98436d2d8",
  "content": "{\"version\":0,\"kind\":\"sell\",\"status\":\"FIAT_SENT\",\"amount\":100,\"fiat_code\":\"XXX\",\"fiat_amount\":1000,\"payment_method\":\"bank transfer\",\"prime\":0}",
  "tags": [],
  "created_at": 1234567890,
  "sig": "a21eb195fe418613aa9a3a8a78039b090e50dc3f9fb06b0f3fe41c63221adc073a9317a1f28d9db843a43c28d860ba173b70132ca85b0e706f6487d43a57ee82"
}
```

## Mostro request release of funds

Mostro send an encrypted message to seller indicating that buyer says that fiat was sent and request to check if fiat was received please release funds, if everything went well, seller respond with a new encrypted message to Mostro with this content to release funds:

```json
{
  "release": true
}
```

## Settle seller's invoice

Mostro settle the invoice and send a replaceable event kind `11000` with the same id, a newer timestamp and status `PAID_HOLD_INVOICE`, right after tries to pay the buyer's invoice, after the invoice is paid Mostro send a replaceable event kind `11000` with the same id, a newer timestamp and status `SUCCESS`
