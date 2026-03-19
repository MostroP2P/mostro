# RPC Rate Limiting — ValidateDbPassword

## Overview

The gRPC method **`ValidateDbPassword`** (protobuf RPC) is a **backward-compatibility** stub: the SQLite database is **not** encrypted, the request **`password`** field is **ignored**, and the response is always success after a per-IP gate.

The implementation is **`validate_db_password`** in `src/rpc/service.rs`. It:

1. Resolves the client address and runs **`check_rate_limit`** on the shared in-memory **`RateLimiter`** (`src/rpc/rate_limiter.rs`).
2. Drops **`password`** on the floor (`let _ = req.password;`).
3. Calls **`record_success`** on the limiter for that IP (clears any tracked state for that key).
4. Returns **`ValidateDbPasswordResponse`** with `success: true`.

**Important:** **`validate_db_password` does not call `record_failure`.** So exponential backoff and lockout described below are **generic `RateLimiter` capabilities** (used by unit tests and available if another call site ever records failures). They are **not** driven by repeated **`ValidateDbPassword`** calls with “wrong passwords,” because passwords are not validated.

See [Issue #569](https://github.com/MostroP2P/mostro/issues/569) for background.

## `ValidateDbPassword` ↔ code map

| Concept | Where |
|--------|--------|
| RPC name | `ValidateDbPassword` in `proto/admin.proto` |
| Handler | `validate_db_password` in `src/rpc/service.rs` |
| Limiter | `password_rate_limiter: Arc<RateLimiter>` on `AdminServiceImpl` |
| Success path | `record_success(&remote_addr)` after ignoring `password` |

## Generic `RateLimiter` behavior (`src/rpc/rate_limiter.rs`)

The in-memory limiter is keyed by client IP. It exposes **`check_rate_limit`**, **`record_failure`**, and **`record_success`**.

**When `record_failure` is used** (e.g. in unit tests, or a hypothetical future handler), the limiter can apply exponential backoff and lockout:

| Failed attempts (`record_failure`) | Effect |
|-----------------------------------|--------|
| 1st | Immediate + 1s delay |
| 2nd | Immediate + 2s delay |
| 3rd | Immediate + 4s delay |
| 4th | Immediate + 8s delay |
| 5th+ | **Locked out for 5 minutes** |

After **`record_success`**, that IP’s failure state is cleared (see `record_success` in `rate_limiter.rs`).

**Not exercised by `ValidateDbPassword` today:** the **`validate_db_password`** handler never invokes **`record_failure`**, so clients only exercising this RPC do not accumulate “failed attempts” through wrong passwords.

### Constants (`src/rpc/rate_limiter.rs`)

```rust
const MAX_ATTEMPTS: u32 = 5;
const LOCKOUT_DURATION: Duration = Duration::from_secs(300); // 5 minutes
const BASE_DELAY_MS: u64 = 1000; // 1 second
```

### Thread safety

The limiter uses `tokio::sync::Mutex`. The lock is dropped before the exponential backoff sleep inside **`record_failure`** so the mutex is not held across the delay.

## Audit logging

- **`validate_db_password`** logs receipt of the RPC at **INFO** (client IP).
- **`RateLimiter`** may emit **WARN** for backoff/lockout when **`check_rate_limit`** denies an IP that already has failure state, or when **`record_failure`** runs — paths that matter for **unit tests** and for **generic** use of the limiter, not for password validation on **`ValidateDbPassword`**.

## Security layers (historical issue checklist)

How the original issue’s ideas map to the codebase today:

| Suggestion | Notes |
|-----------|--------|
| Per-IP rate limiting | **`check_rate_limit`** runs before the handler body. |
| Exponential backoff / lockout | Implemented inside **`RateLimiter`**; **not** triggered by **`ValidateDbPassword`** (no **`record_failure`**). |
| Audit logging | **tracing** in service + limiter. |
| Localhost-only | Default RPC bind **`127.0.0.1`** (see `settings.toml` / `docs/RPC.md`). |
| Strong auth | Out of scope for this stub; would need API keys or similar. |

## Testing

Unit tests in **`src/rpc/rate_limiter.rs`** exercise **`record_failure`**, lockout, **`record_success`**, and eviction — they document **limiter** behavior, not password checking.

## Related

- Issue: [#569](https://github.com/MostroP2P/mostro/issues/569)
- RPC docs: `docs/RPC.md`
- Default RPC config binds to `127.0.0.1:50051` (localhost only)
