# BOLT12 Offer Payouts via LNDK

Mostro optionally supports paying buyer payouts to BOLT12 offers (`lno1…`) by
talking to an [LNDK](https://github.com/lndk-org/lndk) daemon that runs
alongside LND. This page documents the operator setup.

> **Experimental.** BOLT12, LND's onion messaging support, and LNDK itself
> are all still maturing. Leave `lndk_enabled = false` unless you understand
> the risks and want to opt in.

## Architecture

```
Buyer  ---- Nostr ---->  Mostro  ----gRPC---->  LNDK  ----gRPC---->  LND
(sends lno1…)             |                      |                    |
                          |                      |                    |
                          +--BOLT11/LNURL path---|------gRPC--------->+
                                                 |
                                                 +- onion messaging -+
                                                    (via LND custom
                                                     message 513)
```

- LND handles everything Mostro always did: hold invoices, routing, channels.
- LNDK is a thin shim that uses LDK's BOLT12 implementation to build
  `invoice_request` messages, fetch invoices from offer issuers, and hand
  payable invoices back to LND.
- Mostro calls LNDK's gRPC only for the buyer-payout step when
  `order.buyer_invoice` is a BOLT12 offer. All other payment paths are
  untouched.

## Prerequisites

### LND ≥ v0.18.0 with the right build tags

Build (or install a package built) with the subservers LNDK needs:

```sh
make install tags="peersrpc signrpc walletrpc"
```

### LND startup flags

LND must advertise and forward onion messages. Add to `lnd.conf`:

```
[protocol]
protocol.custom-message=513
protocol.custom-nodeann=39
protocol.custom-init=39
```

Or pass them on the command line:

```sh
lnd --protocol.custom-message=513 \
    --protocol.custom-nodeann=39 \
    --protocol.custom-init=39
```

On startup Mostro checks the LND feature bits and logs a warning if these
flags appear to be missing.

### Install and run LNDK

Follow LNDK's own setup guide at
<https://github.com/lndk-org/lndk#setting-up-lndk>. In short:

```sh
git clone https://github.com/lndk-org/lndk
cd lndk
cargo run --bin=lndk -- \
    --address=https://127.0.0.1:10009 \
    --cert-path=/path/to/.lnd/tls.cert \
    --macaroon-path=/path/to/.lnd/data/chain/bitcoin/mainnet/admin.macaroon
```

By default LNDK writes its self-signed TLS cert to `~/.lndk/data/tls-cert.pem`
and listens on `https://127.0.0.1:7000`.

### Bake a custom macaroon for Mostro

Use a minimally scoped macaroon instead of `admin.macaroon`:

```sh
lncli bakemacaroon --save_to=/path/to/mostro-lndk.macaroon \
  uri:/walletrpc.WalletKit/DeriveKey \
  uri:/signrpc.Signer/SignMessage \
  uri:/lnrpc.Lightning/GetNodeInfo \
  uri:/lnrpc.Lightning/ConnectPeer \
  uri:/lnrpc.Lightning/GetInfo \
  uri:/lnrpc.Lightning/ListPeers \
  uri:/lnrpc.Lightning/GetChanInfo \
  uri:/lnrpc.Lightning/QueryRoutes \
  uri:/routerrpc.Router/SendToRouteV2 \
  uri:/routerrpc.Router/TrackPaymentV2
```

## Mostro configuration

In `settings.toml`, fill in the `[lightning]` LNDK fields:

```toml
[lightning]
# ... existing LND settings ...

lndk_enabled = true
lndk_grpc_host = "https://127.0.0.1:7000"
lndk_cert_file = "/home/mostro/.lndk/data/tls-cert.pem"
lndk_macaroon_file = "/home/mostro/mostro-lndk.macaroon"
lndk_fetch_invoice_timeout = 60
# lndk_fee_limit_percent = 0.2  # fraction (0.002 = 0.2%). Defaults to mostro.max_routing_fee.
```

On startup Mostro will:

1. Try to dial LNDK. If it cannot (wrong cert, unreachable, bad macaroon),
   startup aborts — BOLT12 is opt-in, so silently dropping it is worse than
   refusing to start.
2. Log whether LND's onion-message feature bits are advertised. If not, a
   loud warning is emitted but startup continues.

## What Mostro does with an offer

When a buyer sends a BOLT12 offer string as their payout destination:

1. **Validation (at `add-invoice` / `take-sell` time).** `is_valid_invoice`
   decodes the offer with the `lightning` crate and rejects:
   - non-BTC currency offers (e.g. USD);
   - offers whose pinned amount disagrees with `order.amount - order.fee`;
   - offers whose `absolute_expiry` has already elapsed;
   - offers that cannot satisfy a single-item purchase;
   - offers received while `lndk_enabled = false`.
2. **Payout (`do_payment`).** Mostro calls LNDK's `GetInvoice` to fetch a
   fresh BOLT12 invoice bound to the offer, **re-validates** the fetched
   invoice's amount and expiry (LNDK's `PayOffer` shortcut does not), then
   calls `PayInvoice`. On success the returned preimage transitions the
   order to `Success`. On failure the order enters the normal failed-payment
   retry loop.

## BIP-353 resolution (`user@domain` → BOLT12 offer)

When `bip353_enabled = true`, Mostro resolves human-readable
`user@domain.tld` payout targets to BOLT12 offers via DNSSEC-validated DNS
TXT records (BIP-353 / `_bitcoin-payment` zone). On a successful resolve,
the original address is replaced with the resolved offer at order creation
time and the BOLT12 payout path described above takes over. On any failure
(no record, DNSSEC fails, malformed URI), Mostro falls back to the LNURL
path so existing Lightning Addresses keep working.

Configuration:

```toml
[lightning]
bip353_enabled = true
# Any DoH resolver supporting RFC 8484's JSON API works. Default below.
bip353_doh_resolver = "https://1.1.1.1/dns-query"
# Skip DNSSEC AD-flag check. DANGER: regtest only.
bip353_skip_dnssec = false
```

Notes:

- BIP-353 requires `lndk_enabled = true`; otherwise resolution is skipped
  silently because the resolved offer would be unpayable.
- DNSSEC validation is enforced via the resolver's `AD` flag. Disabling
  `bip353_skip_dnssec` in production lets DNS-level attackers redirect
  payouts.
- Resolution is best-effort: a DoH timeout or non-DNSSEC response is
  treated as "no record" so the LNURL path can still serve the request.

## Limitations

- **BOLT12 invoices (`lni1…`) as direct inputs are rejected.** Users must
  send the offer, not a pre-fetched invoice.
- **Offer creation is not supported.** Mostro does not issue BOLT12 offers
  for dev-fee receipt; dev fees still use a BOLT11 destination from config.
- **No background retries for BOLT12 yet.** Offer reusability makes this
  trivial to add in a follow-up, but for now BOLT12 payment failures follow
  the same retry cadence as BOLT11 and still surface an `AddInvoice`
  request to the buyer after the configured retry budget is exhausted.
- **Onion-message network reachability is still maturing.** BOLT12 fetches
  can fail in ways BOLT11 does not — check Mostro logs for `lndk
  get_invoice:` errors if you see unexpected BOLT12 failures.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `LNDK initialization failed: failed to read LNDK TLS cert` | `lndk_cert_file` path wrong or file not readable by the Mostro user |
| `LNDK initialization failed: failed to connect to LNDK` | LNDK daemon not running, or `lndk_grpc_host` mismatch |
| `LNDK initialization failed: TLS config` | Cert file is not a valid PEM certificate |
| Warning: `LND does not advertise onion-message support` | Missing `--protocol.custom-message=513 --protocol.custom-nodeann=39 --protocol.custom-init=39` on LND |
| `lndk get_invoice: ...` errors in logs during payout | Offer issuer unreachable, network has no onion-message route, or the offer expired |
| `BOLT12 invoice amount mismatch` | The offer issuer returned an invoice that does not match the requested amount — defense-in-depth aborted the payment |

## Disabling BOLT12

Set `lndk_enabled = false` and restart. Existing orders whose
`buyer_invoice` is a BOLT12 offer will fail their next payout attempt with
`BOLT12 offer received but LNDK is disabled` and enter the usual retry
loop. Consider waiting until all in-flight BOLT12 orders drain before
flipping the flag.
