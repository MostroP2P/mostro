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

/// Nostr event kind for rating events (NIP-33 replaceable)
/// Kind 38384 is in the parameterized replaceable events range (30000-39999)
pub const NOSTR_RATING_EVENT_KIND: u16 = 38384;

/// Nostr event kind for mostro info events (NIP-33 replaceable)
/// Kind 38385 is in the parameterized replaceable events range (30000-39999)
pub const NOSTR_INFO_EVENT_KIND: u16 = 38385;

/// Nostr event kind for dispute events (NIP-33 replaceable)
/// Kind 38386 is in the parameterized replaceable events range (30000-39999)
pub const NOSTR_DISPUTE_EVENT_KIND: u16 = 38386;
