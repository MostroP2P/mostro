//! String-backed enums persisted in the `bonds` table.
//!
//! These are the daemon-internal counterparts to the `[anti_abuse_bond]`
//! configuration. We stringify for storage rather than using an integer
//! discriminant so that a DB dump remains readable by operators.

use std::fmt;
use std::str::FromStr;

/// Which side of a trade a bond row represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondRole {
    Maker,
    Taker,
}

impl fmt::Display for BondRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BondRole::Maker => f.write_str("maker"),
            BondRole::Taker => f.write_str("taker"),
        }
    }
}

impl FromStr for BondRole {
    type Err = BondParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "maker" => Ok(BondRole::Maker),
            "taker" => Ok(BondRole::Taker),
            other => Err(BondParseError::UnknownRole(other.to_string())),
        }
    }
}

/// Lifecycle states for a bond row.
///
/// The state machine is intentionally narrow:
///
/// ```text
///  Requested ──► Locked ──┬──► Released (happy / cancelled-before-timeout)
///                         └──► PendingPayout ──┬──► Slashed    (winner paid)
///                                              ├──► Forfeited  (winner never claimed in window)
///                                              └──► Failed ──► (back to PendingPayout
///                                                              on fresh AddBondInvoice
///                                                              within claim window)
/// ```
///
/// `Forfeited` is a hard terminal state — the counterparty never
/// submitted a payout invoice within `payout_claim_window_days`, the
/// node retains `amount_sats` in full, no operator action needed.
///
/// `Failed` is reachable when `send_payment` exhausts `payout_max_retries`
/// against a delivered invoice. Unlike the other "terminal" states it
/// is *user-recoverable*: a fresh [`Action::AddBondInvoice`] from the
/// recipient resurrects the bond back to `PendingPayout` (resets
/// `payout_attempts` / `invoice_request_attempts`, overwrites
/// `payout_invoice`) so long as `now - slashed_at <
/// payout_claim_window_days * 86_400`. Past the claim window, the row
/// stays `Failed` and requires operator attention to pay the
/// counterparty manually.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondState {
    /// Hold invoice created; waiting for the bonded party to pay it so LND
    /// reports `Accepted`.
    Requested,
    /// Hold invoice accepted by LND. The bond is in force.
    Locked,
    /// Hold invoice was cancelled (not slashed). Terminal happy exit.
    Released,
    /// A slash condition was hit. The bond hold invoice has already been
    /// **settled** (claimed into Mostro's wallet at slash time by Phase 2);
    /// Phase 3 is asynchronously driving the recipient payout
    /// (`AddBondInvoice` → `send_payment` → retries / forfeiture).
    PendingPayout,
    /// Winner paid successfully. Terminal.
    Slashed,
    /// `payout_claim_window_days` elapsed without the counterparty ever
    /// submitting a payout invoice; the node retains `amount_sats` in
    /// full. Terminal — designed-in long-stop, no operator action needed.
    Forfeited,
    /// `send_payment` retries exhausted on a delivered payout invoice.
    /// User-recoverable: a fresh [`Action::AddBondInvoice`] from the
    /// recipient transitions the row back to `PendingPayout` while
    /// `now - slashed_at < payout_claim_window_days * 86_400`. Past
    /// the claim window, requires operator attention to pay the
    /// counterparty manually.
    Failed,
}

impl BondState {
    /// True for states that the Phase 1 release / cancel paths must
    /// not transition out of: the LND HTLC has already been settled or
    /// cancelled, so any further release attempt is at best a no-op
    /// and at worst a double-action against LND. Used so call sites
    /// don't have to enumerate `Released | Slashed | Forfeited |
    /// Failed` manually.
    ///
    /// Scope is intentionally release-flow only. `Failed` is included
    /// because, like the other three, its HTLC has been disposed of
    /// (settled at slash time) — but Phase 3's `add_bond_invoice_action`
    /// can still flip a `Failed` row back to `PendingPayout` on a
    /// fresh user invoice within the claim window. That recovery path
    /// works on the DB row directly and does not go through these
    /// release/cancel helpers.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            BondState::Released | BondState::Slashed | BondState::Forfeited | BondState::Failed
        )
    }

    /// True for states that still have an outstanding LND HTLC and are
    /// candidates for release / slash. Inverse of [`BondState::is_terminal`]
    /// minus `PendingPayout`, which is owned by the Phase 3 payout job
    /// rather than the release flow.
    pub fn is_active(self) -> bool {
        matches!(self, BondState::Requested | BondState::Locked)
    }
}

impl fmt::Display for BondState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BondState::Requested => "requested",
            BondState::Locked => "locked",
            BondState::Released => "released",
            BondState::PendingPayout => "pending-payout",
            BondState::Slashed => "slashed",
            BondState::Forfeited => "forfeited",
            BondState::Failed => "failed",
        };
        f.write_str(s)
    }
}

impl FromStr for BondState {
    type Err = BondParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "requested" => Ok(BondState::Requested),
            "locked" => Ok(BondState::Locked),
            "released" => Ok(BondState::Released),
            "pending-payout" => Ok(BondState::PendingPayout),
            "slashed" => Ok(BondState::Slashed),
            "forfeited" => Ok(BondState::Forfeited),
            "failed" => Ok(BondState::Failed),
            other => Err(BondParseError::UnknownState(other.to_string())),
        }
    }
}

/// Why a bond transitioned to `PendingPayout` / `Slashed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BondSlashReason {
    /// Bonded party lost the dispute (Phase 2 / 5).
    LostDispute,
    /// Bonded party let the waiting-state timeout actually elapse
    /// (Phase 4 / 7). A cancellation before the timeout is NEVER this.
    Timeout,
}

impl fmt::Display for BondSlashReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BondSlashReason::LostDispute => f.write_str("lost-dispute"),
            BondSlashReason::Timeout => f.write_str("timeout"),
        }
    }
}

impl FromStr for BondSlashReason {
    type Err = BondParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "lost-dispute" => Ok(BondSlashReason::LostDispute),
            "timeout" => Ok(BondSlashReason::Timeout),
            other => Err(BondParseError::UnknownSlashReason(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BondParseError {
    UnknownRole(String),
    UnknownState(String),
    UnknownSlashReason(String),
}

impl fmt::Display for BondParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BondParseError::UnknownRole(v) => write!(f, "unknown bond role: {v}"),
            BondParseError::UnknownState(v) => write!(f, "unknown bond state: {v}"),
            BondParseError::UnknownSlashReason(v) => {
                write!(f, "unknown bond slash reason: {v}")
            }
        }
    }
}

impl std::error::Error for BondParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_roundtrip() {
        for r in [BondRole::Maker, BondRole::Taker] {
            assert_eq!(BondRole::from_str(&r.to_string()).unwrap(), r);
        }
    }

    #[test]
    fn state_roundtrip() {
        for s in [
            BondState::Requested,
            BondState::Locked,
            BondState::Released,
            BondState::PendingPayout,
            BondState::Slashed,
            BondState::Forfeited,
            BondState::Failed,
        ] {
            assert_eq!(BondState::from_str(&s.to_string()).unwrap(), s);
        }
    }

    #[test]
    fn slash_reason_roundtrip() {
        for s in [BondSlashReason::LostDispute, BondSlashReason::Timeout] {
            assert_eq!(BondSlashReason::from_str(&s.to_string()).unwrap(), s);
        }
    }

    #[test]
    fn unknown_parse_rejected() {
        assert!(BondRole::from_str("solver").is_err());
        assert!(BondState::from_str("in-progress").is_err());
        assert!(BondSlashReason::from_str("whatever").is_err());
    }

    #[test]
    fn parse_error_display_and_error_trait() {
        // Every `BondParseError` variant must render its offending value
        // so operator logs can pinpoint the corrupted column. The enum
        // also implements `std::error::Error`, so it must be usable as a
        // trait object.
        let role_err = BondRole::from_str("solver").unwrap_err();
        assert_eq!(role_err.to_string(), "unknown bond role: solver");

        let state_err = BondState::from_str("in-progress").unwrap_err();
        assert_eq!(state_err.to_string(), "unknown bond state: in-progress");

        let reason_err = BondSlashReason::from_str("whatever").unwrap_err();
        assert_eq!(
            reason_err.to_string(),
            "unknown bond slash reason: whatever"
        );

        let boxed: Box<dyn std::error::Error> = Box::new(role_err);
        assert!(boxed.source().is_none());
    }

    #[test]
    fn terminal_and_active_helpers() {
        for s in [
            BondState::Released,
            BondState::Slashed,
            BondState::Forfeited,
            BondState::Failed,
        ] {
            assert!(s.is_terminal(), "{s} should be terminal");
            assert!(!s.is_active(), "{s} should not be active");
        }
        for s in [BondState::Requested, BondState::Locked] {
            assert!(s.is_active(), "{s} should be active");
            assert!(!s.is_terminal(), "{s} should not be terminal");
        }
        // `PendingPayout` is neither: it's owned by the payout job.
        assert!(!BondState::PendingPayout.is_terminal());
        assert!(!BondState::PendingPayout.is_active());
    }
}
