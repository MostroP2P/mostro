# RPC Rate Limiting — ValidateDbPassword

## Overview

The `ValidateDbPassword` RPC endpoint is protected against brute-force attacks
with an in-memory rate limiter that tracks failed attempts per client IP.

## Problem

The `ValidateDbPassword` endpoint accepts a password and validates it against the
stored admin hash. Without protection, an attacker with network access to the RPC
interface could systematically try passwords at thousands of attempts per second.

See [Issue #569](https://github.com/MostroP2P/mostro/issues/569) for full details.

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

The `validate_db_password` method now:

1. Extracts the client's remote address from the gRPC request
2. Checks the rate limiter — returns `RESOURCE_EXHAUSTED` if locked out
3. Validates the password against the stored hash
4. On failure: records the attempt (triggers exponential backoff delay)
5. On success: resets the client's failure state

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
