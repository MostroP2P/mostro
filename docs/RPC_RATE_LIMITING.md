# RPC Rate Limiting — ValidateDbPassword

## Overview

The `ValidateDbPassword` RPC is a **backward-compatibility** stub: the database is
not encrypted, the `password` field is **ignored**, and the handler always returns
success after an initial per-IP check in `validate_db_password` (`src/rpc/service.rs`).

An in-memory rate limiter (`src/rpc/rate_limiter.rs`) runs **before** the handler
processes the request. It can enforce backoff or lockout for a client IP when the
limiter’s failure state is used; the current handler path records **success** only
and does not validate passwords.

See [Issue #569](https://github.com/MostroP2P/mostro/issues/569) for background.

## Implementation

### Rate Limiter (`src/rpc/rate_limiter.rs`)

A lightweight, in-memory rate limiter keyed by client IP address. No external
dependencies required — uses only `tokio::sync::Mutex` and `std::collections::HashMap`.

**Behavior:**

| Failed Attempts | Response |
|----------------|----------|
| 1st | Immediate + 1s delay |
| 2nd | Immediate + 2s delay |
| 3rd | Immediate + 4s delay |
| 4th | Immediate + 8s delay |
| 5th+ | **Locked out for 5 minutes** |

After a successful validation, the client's failure state is reset.

### Integration (`src/rpc/service.rs`)

The `validate_db_password` method:

1. Extracts the client's remote address from the gRPC request
2. Runs `check_rate_limit` — may return `RESOURCE_EXHAUSTED` if the limiter denies the IP
3. Ignores `password` (no database encryption); returns `success: true`
4. Calls `record_success` on the limiter for that IP

### Audit Logging

All attempts are logged via `tracing`:

- **Rate-limited requests:** `WARN` with client IP
- **Failed attempts:** `WARN` with client IP and attempt count
- **Lockouts:** `WARN` with client IP and lockout duration

### Security Layers

This implementation addresses the issue's suggestions:

| Suggestion | Status | Notes |
|-----------|--------|-------|
| Rate limiting | ✅ | Per-IP tracking with exponential backoff |
| Exponential backoff | ✅ | 1s → 2s → 4s → 8s → lockout |
| Lockout | ✅ | 5-minute lockout after 5 failures |
| Audit logging | ✅ | All attempts logged via tracing |
| Localhost-only | ℹ️ | Default config already binds to `127.0.0.1` |
| Auth requirement | ℹ️ | Out of scope — would require session/API key infra |

### Constants

Configurable via constants in `src/rpc/rate_limiter.rs`:

```rust
const MAX_ATTEMPTS: u32 = 5;
const LOCKOUT_DURATION: Duration = Duration::from_secs(300); // 5 minutes
const BASE_DELAY_MS: u64 = 1000; // 1 second
```

### Thread Safety

The rate limiter uses `tokio::sync::Mutex` for async-safe access. The lock is
dropped before applying the exponential backoff sleep to avoid holding it during
the delay.

## Testing

Unit tests in `src/rpc/rate_limiter.rs` verify:

- First attempt is always allowed
- Lockout triggers after `MAX_ATTEMPTS` failures
- Success resets the failure state
- Different IPs are tracked independently

## Related

- Issue: [#569](https://github.com/MostroP2P/mostro/issues/569)
- RPC docs: `docs/RPC.md`
- Default RPC config binds to `127.0.0.1:50051` (localhost only)
