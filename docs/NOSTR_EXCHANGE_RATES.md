# Nostr-Based Exchange Rates

## Overview

Mostro daemon publishes Bitcoin/fiat exchange rates to Nostr relays as NIP-33 addressable events (kind `30078`). This enables:

- **Censorship resistance** — Mobile clients in censored regions (Venezuela, Cuba, etc.) can fetch rates via Nostr
- **Zero scaling cost** — Relays distribute events; no per-request infrastructure needed
- **Backward compatibility** — HTTP API remains available as fallback

---

## Event Structure

### Kind 30078 (NIP-33 Addressable Event)

```json
{
  "kind": 30078,
  "pubkey": "82fa8cb978b43c79b2156585bac2c011176a21d2aead6d9f7c575c005be88390",
  "created_at": 1732546800,
  "tags": [
    ["d", "mostro-rates"],
    ["updated_at", "1732546800"],
    ["source", "yadio"],
    ["expiration", "1732550400"]
  ],
  "content": "{\"USD\": {\"BTC\": 0.000024}, \"EUR\": {\"BTC\": 0.000022}, ...}",
  "sig": "..."
}
```

### Fields

- **kind:** `30078` (application-specific data, NIP-33 replaceable)
- **pubkey:** Mostro daemon's public key (same key that signs orders)
- **d tag:** `"mostro-rates"` (NIP-33 identifier — replaces previous rate events)
- **updated_at tag:** Unix timestamp of last update
- **source tag:** `"yadio"` (indicates rate source)
- **expiration tag:** Unix timestamp 1 hour after creation (NIP-40) — prevents stale rates
- **content:** JSON-encoded rates in format `{"CURRENCY": {"BTC": rate}, ...}`

### Content Format

The `content` field contains a JSON object mapping currency codes to BTC rates:

```json
{
  "USD": { "BTC": 0.000024 },
  "EUR": { "BTC": 0.000022 },
  "VES": { "BTC": 0.0000000012 },
  "ARS": { "BTC": 0.0000000095 }
}
```

**Rate semantics:** Each value represents how much BTC equals 1 unit of fiat currency.

**Example:** `"USD": {"BTC": 0.000024}` means 1 USD = 0.000024 BTC (≈41,666 USD/BTC).

---

## Configuration

### Enable/Disable Publishing

Add to `settings.toml`:

```toml
[mostro]
# ... existing config ...

# Publish exchange rates to Nostr (default: true)
publish_exchange_rates_to_nostr = true

# Exchange rates update interval in seconds (default: 300 = 5 minutes)
exchange_rates_update_interval_seconds = 300
```

**Defaults:**
- `publish_exchange_rates_to_nostr`: `true` (enabled for censorship resistance)
- `exchange_rates_update_interval_seconds`: `300` (5 minutes)

### Update Frequency

Exchange rates are fetched from Yadio API and published to Nostr based on the configured `exchange_rates_update_interval_seconds` value.

**Recommended values:**
- **Production:** `300` (5 minutes) — balances freshness with API rate limits
- **Development:** `60` (1 minute) — faster testing
- **Low-volume instances:** `600` (10 minutes) — reduces API calls

**Note:** Very short intervals (&lt;60s) may hit Yadio API rate limits.

---

## Implementation Details

### Code Flow

1. **Scheduler** (`scheduler.rs`): `job_update_bitcoin_prices()` runs every 300 seconds
2. **BitcoinPriceManager** (`bitcoin_price.rs`): 
   - Fetches rates from Yadio HTTP API
   - Updates in-memory cache
   - If `publish_exchange_rates_to_nostr == true`:
     - Transforms rates to expected JSON format
     - Creates NIP-33 event (kind `30078`)
     - Publishes to configured Nostr relays

3. **Event Creation** (`nip33.rs`): `new_exchange_rates_event()` creates the signed event

### Error Handling

- **Yadio API failure** → Logs warning, skips update (keeps previous rates valid)
- **Nostr publish failure** → Logs error but doesn't fail the update job
- **Event creation failure** → Logs error but doesn't crash daemon

**Philosophy:** Nostr publishing is best-effort; HTTP API remains the source of truth.

---

## Security Considerations

### Event Verification (Client-Side)

Mobile clients **MUST** verify the event `pubkey` matches the connected Mostro instance's pubkey to prevent price manipulation attacks.

**Attack scenario:** Malicious actor publishes fake rates to influence order creation.

**Mitigation:** Clients only accept rate events signed by their connected Mostro instance.

See: [Mobile client spec](https://github.com/MostroP2P/app/blob/main/.specify/NOSTR_EXCHANGE_RATES.md)

### Relay Security

- Events are signed with Mostro's private key (standard NIP-01 signature verification)
- NIP-33 addressable events: newer events replace older ones (prevents stale data)
- **NIP-40 expiration:** Events expire after 1 hour (relays should delete them)
- No sensitive data in events (all rates are public information)

---

## Testing

### Unit Tests

```bash
cargo test bitcoin_price
```

**Coverage:**
- Yadio API response deserialization
- Rate format transformation (`{"USD": 0.024}` → `{"USD": {"BTC": 0.024}}`)
- JSON serialization for Nostr event content

### Integration Testing

1. Start Mostro daemon with `publish_exchange_rates_to_nostr = true`
2. Wait 5 minutes (or trigger update manually)
3. Query relay for kind `30078` events from Mostro pubkey:

```bash
# Using nak CLI
nak req -k 30078 -a <mostro_pubkey> --tag d=mostro-rates wss://relay.mostro.network
```

**Expected output:** JSON event with current exchange rates

### Manual Testing

```bash
# Subscribe to rate updates
nostcat -sub -k 30078 -a <mostro_pubkey> wss://relay.mostro.network

# Verify content format
echo '<event_content>' | jq .
# Should output: {"USD": {"BTC": 0.000024}, ...}
```

---

## Deployment

### Production Checklist

- [ ] Verify `publish_exchange_rates_to_nostr` config in `settings.toml`
- [ ] Set `exchange_rates_update_interval_seconds` (default: 300)
- [ ] Confirm Nostr relays are reachable from daemon
- [ ] Monitor logs for "Starting Bitcoin price update job (interval: Xs)" on startup
- [ ] Monitor logs for "Exchange rates published to Nostr" messages
- [ ] Test client-side rate fetching from Nostr
- [ ] Verify fallback to HTTP API works if Nostr unavailable

### Monitoring

**Success indicators:**
```
INFO Exchange rates published to Nostr. Event ID: <id> (<N> currencies)
```

**Error indicators:**
```
ERROR Failed to publish exchange rates to Nostr: <error>
ERROR Failed to send exchange rates event to relays: <error>
```

---

## Related Documentation

- [NIP-33: Parameterized Replaceable Events](https://github.com/nostr-protocol/nips/blob/master/33.md)
- [Mobile Client Spec](https://github.com/MostroP2P/app/blob/main/.specify/NOSTR_EXCHANGE_RATES.md)
- [Issue #684: Feature Proposal](https://github.com/MostroP2P/mostro/issues/684)

---

## Future Enhancements

### Multi-Source Aggregation

Aggregate rates from multiple sources (Yadio, CoinGecko, Binance):

```toml
[mostro]
exchange_rate_sources = ["yadio", "coingecko", "binance"]
```

Publish average or median rates to reduce single-source dependency.

### Rate History

Store historical rates in database:

```sql
CREATE TABLE exchange_rate_history (
    timestamp INTEGER PRIMARY KEY,
    currency TEXT NOT NULL,
    btc_rate REAL NOT NULL,
    source TEXT NOT NULL
);
```

Publish daily/weekly summaries as separate NIP-33 events.

### Custom Event Kinds

Propose standardized Nostr event kind for exchange rates (currently using generic `30078`).

**Draft NIP:** "Exchange Rate Events" (kind TBD, e.g., `30400`)
