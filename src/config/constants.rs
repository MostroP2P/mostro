/// Development fee configuration constants
/// Minimum development fee percentage (10% of Mostro fee)
pub const MIN_DEV_FEE_PERCENTAGE: f64 = 0.10;

/// Maximum development fee percentage (100% of Mostro fee)
pub const MAX_DEV_FEE_PERCENTAGE: f64 = 1.0;

/// Official Mostro development Lightning Address
pub const DEV_FEE_LIGHTNING_ADDRESS: &str = "pivotaldeborah52@walletofsatoshi.com";

/// Nostr event kind for dev fee payment audit events
/// Kind 8383 is in the regular events range (1000-9999) per NIP-01
/// This ensures events are NOT replaceable, maintaining complete audit history
pub const DEV_FEE_AUDIT_EVENT_KIND: u16 = 8383;

/// NIP-40 retention for dev fee audit events (kind 8383), in days.
/// Audit events document fee payments for third-party verification, so they
/// are kept for a full year — not for the much shorter order-expiration
/// window (`max_expiration_days`).
pub const DEV_FEE_AUDIT_EXPIRATION_DAYS: u32 = 365;

/// Nostr event kind for protocol-v2 direct messages (NIP-44 direct transport)
/// Kind 14 carries Mostro protocol messages as signed events with NIP-44
/// encrypted content when `transport = "nip44"` (see docs/TRANSPORT_V2_SPEC.md)
pub const DM_EVENT_KIND: u16 = 14;

/// Nostr event kind for exchange rates (NIP-33 addressable event)
/// Kind 30078 is in the replaceable events range (30000-39999) per NIP-33
/// This allows the same Mostro instance to publish updated rates that replace previous events
pub const NOSTR_EXCHANGE_RATES_EVENT_KIND: u16 = 30078;

/// Filename of the environment file auto-loaded from the settings directory at
/// startup. Shared between the wizard (writes it) and the loader (reads it).
pub const ENV_FILENAME: &str = ".env";

/// Environment variable name used to override the Nostr private key from the
/// process environment. Shared between the wizard and the loader.
pub const NSEC_ENV_VAR: &str = "MOSTRO_NSEC_PRIVKEY";

/// Environment variable holding the shared bearer token that authenticates
/// admin gRPC callers. Deliberately env-only (like `NSEC_ENV_VAR`): the token
/// never lives in `settings.toml`, only in the process environment or
/// `<settings_dir>/.env`.
pub const RPC_TOKEN_ENV_VAR: &str = "MOSTRO_RPC_TOKEN";

/// Minimum accepted length for `MOSTRO_RPC_TOKEN`. 32 characters is the
/// shortest base64 encoding of 24 random bytes, well past the point where
/// online guessing against a single daemon is meaningful.
pub const MIN_RPC_TOKEN_LEN: usize = 32;

/// Minimum number of distinct characters in `MOSTRO_RPC_TOKEN`. The length
/// floor alone is satisfied by `"a"` repeated 32 times, and the decision not
/// to rate-limit authentication (`crate::rpc::auth`) leans on the token being
/// randomly generated, not merely long. Random tokens clear this easily —
/// `openssl rand -base64 32` yields ~30 distinct characters on average —
/// while hand-typed passphrases do not.
pub const MIN_RPC_TOKEN_DISTINCT_CHARS: usize = 16;
