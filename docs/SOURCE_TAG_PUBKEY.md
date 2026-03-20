# Source Tag: Mostro Pubkey

## Overview

The `source` tag in kind 38383 order events now includes the Mostro daemon's pubkey,
allowing clients to identify which Mostro instance published the order.

## Format

### Before

```
mostro:{order_id}?relays={relay1},{relay2}
```

### After

```
mostro:{order_id}?relays={relay1},{relay2}&mostro={pubkey}
```

### Example

```
mostro:e215c07e-b1f9-45b0-9640-0295067ee99a?relays=wss://relay.mostro.network,wss://nos.lol&mostro=82fa8cb978b43c79b2156585bac2c011176a21d2aead6d9f7c575c005be88390
```

## Backward Compatibility

Clients that do not understand the `mostro` query parameter can safely ignore it.
The `relays` parameter remains in the same position and format as before.

## Client Behavior

When a client receives a deep link with a `mostro` parameter:

1. If the pubkey matches the currently selected Mostro instance → open order directly
2. If different → prompt user to switch instances, then navigate to order detail

See [MostroP2P/mobile#541](https://github.com/MostroP2P/mobile/issues/541) for client implementation.

## Related

- [Order Event spec](https://mostro.network/protocol/order_event.html)
- Issue: [#678](https://github.com/MostroP2P/mostro/issues/678)
