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

/// Nostr event kind for protocol-v2 direct messages (NIP-44 direct transport)
/// Kind 14 carries Mostro protocol messages as signed events with NIP-44
/// encrypted content when `transport = "nip44"` (see docs/TRANSPORT_V2_SPEC.md)
pub const DM_EVENT_KIND: u16 = 14;

/// Nostr event kind for exchange rates (NIP-33 addressable event)
/// Kind 30078 is in the replaceable events range (30000-39999) per NIP-33
/// This allows the same Mostro instance to publish updated rates that replace previous events
pub const NOSTR_EXCHANGE_RATES_EVENT_KIND: u16 = 30078;

/// LND's own default for `invoices.holdexpirydelta`: how many blocks before
/// an accepted hold HTLC's expiry height LND force-cancels the invoice to
/// avoid a channel force-close, refunding the payer.
///
/// mostrod cannot read this value over gRPC, so it is the reference point the
/// escrow-deadline windows below are validated against: the daemon has to act
/// on a trade *before* LND takes the escrow away from it.
pub const LND_DEFAULT_HOLD_EXPIRY_DELTA: u32 = 24;

/// Default `lightning.escrow_expiry_safety_blocks` (~6 h): how close to the
/// escrow HTLC's expiry height a trade may get before mostrod ends it itself.
pub const DEFAULT_ESCROW_EXPIRY_SAFETY_BLOCKS: u32 = 36;

/// Default `lightning.escrow_expiry_warning_blocks` (~12 h): how close a trade
/// may get before mostrod escalates it — for a `fiat-sent` order that means
/// opening the dispute so a solver can still act while the escrow exists.
pub const DEFAULT_ESCROW_EXPIRY_WARNING_BLOCKS: u32 = 72;

/// Filename of the environment file auto-loaded from the settings directory at
/// startup. Shared between the wizard (writes it) and the loader (reads it).
pub const ENV_FILENAME: &str = ".env";

/// Environment variable name used to override the Nostr private key from the
/// process environment. Shared between the wizard and the loader.
pub const NSEC_ENV_VAR: &str = "MOSTRO_NSEC_PRIVKEY";
