//! `[payer_history]` configuration: payment-account history /
//! anti-triangulation (`docs/PAYER_HISTORY_ANTI_TRIANGULATION.md`).

use serde::{Deserialize, Serialize};

/// Default `experienced_min_trades` (D-7): successful, undisputed trades a
/// seller must already have with *other* buyers to count as experienced.
pub const DEFAULT_EXPERIENCED_MIN_TRADES: u32 = 5;
/// Default `experienced_min_days` (D-7): days since that seller's first
/// such trade.
pub const DEFAULT_EXPERIENCED_MIN_DAYS: u32 = 30;

/// Payment-account history (anti-triangulation). Opt-in; when `enabled`
/// is false the feature is inert in the sense of D-10: no new action,
/// message, tag or history row.
/// See `docs/PAYER_HISTORY_ANTI_TRIANGULATION.md`.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct PayerHistorySettings {
    /// Master switch. Absent section or `false` ≡ disabled.
    #[serde(default)]
    pub enabled: bool,
    /// Reject `fiat-sent` when no `declare-payer` was received for the
    /// order. Has no effect unless `enabled` is also true.
    #[serde(default)]
    pub require_declaration: bool,
    /// "Experienced counterparty" trade threshold (D-7). Node policy,
    /// advertised on the info event; NOT part of the protocol.
    #[serde(default = "default_experienced_min_trades")]
    pub experienced_min_trades: u32,
    /// "Experienced counterparty" age threshold in days (D-7).
    #[serde(default = "default_experienced_min_days")]
    pub experienced_min_days: u32,
}

const fn default_experienced_min_trades() -> u32 {
    DEFAULT_EXPERIENCED_MIN_TRADES
}

const fn default_experienced_min_days() -> u32 {
    DEFAULT_EXPERIENCED_MIN_DAYS
}

// `Default` is implemented by hand, NOT derived: `#[serde(default = "...")]`
// only runs during deserialization, so a derived `Default` would silently
// produce `experienced_min_trades = 0` / `experienced_min_days = 0`, a
// policy where every counterparty qualifies as experienced. The two helpers
// above are the single source of truth for both paths.
impl Default for PayerHistorySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            require_declaration: false,
            experienced_min_trades: default_experienced_min_trades(),
            experienced_min_days: default_experienced_min_days(),
        }
    }
}

impl PayerHistorySettings {
    /// `true` when the feature runs. Takes the optional section so callers
    /// can pass `settings.payer_history.as_ref()` straight through.
    pub fn enabled(cfg: Option<&Self>) -> bool {
        cfg.is_some_and(|c| c.enabled)
    }

    /// `true` when `fiat-sent` must be preceded by `declare-payer`.
    /// Implies [`PayerHistorySettings::enabled`]: `require_declaration` on a
    /// disabled section gates nothing (D-10).
    pub fn declaration_required(cfg: Option<&Self>) -> bool {
        cfg.is_some_and(|c| c.enabled && c.require_declaration)
    }

    /// `(experienced_min_trades, experienced_min_days)`, falling back to the
    /// documented defaults when the section is absent.
    pub fn experience_thresholds(cfg: Option<&Self>) -> (u32, u32) {
        cfg.map_or(
            (DEFAULT_EXPERIENCED_MIN_TRADES, DEFAULT_EXPERIENCED_MIN_DAYS),
            |c| (c.experienced_min_trades, c.experienced_min_days),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Stub {
        #[serde(default)]
        payer_history: Option<PayerHistorySettings>,
    }

    #[test]
    fn payer_history_defaults_match_documented_thresholds() {
        // Arrange / Act
        let from_default = PayerHistorySettings::default();
        let from_serde: PayerHistorySettings = toml::from_str("").unwrap();

        // Assert
        assert_eq!(from_default.experienced_min_trades, 5);
        assert_eq!(from_default.experienced_min_days, 30);
        assert_eq!(from_serde.experienced_min_trades, 5);
        assert_eq!(from_serde.experienced_min_days, 30);
        assert!(!from_default.enabled);
        assert!(!from_serde.enabled);
        assert_eq!(from_default, from_serde);
    }

    #[test]
    fn absent_section_is_disabled() {
        let parsed: Stub = toml::from_str("").unwrap();
        assert!(parsed.payer_history.is_none());
        assert!(!PayerHistorySettings::enabled(
            parsed.payer_history.as_ref()
        ));
        assert!(!PayerHistorySettings::declaration_required(
            parsed.payer_history.as_ref()
        ));
        assert_eq!(
            PayerHistorySettings::experience_thresholds(parsed.payer_history.as_ref()),
            (5, 30)
        );
    }

    #[test]
    fn empty_section_is_disabled() {
        let parsed: Stub = toml::from_str("[payer_history]").unwrap();
        assert!(!PayerHistorySettings::enabled(
            parsed.payer_history.as_ref()
        ));
    }

    #[test]
    fn require_declaration_without_enabled_gates_nothing() {
        let parsed: Stub = toml::from_str("[payer_history]\nrequire_declaration = true").unwrap();
        assert!(!PayerHistorySettings::declaration_required(
            parsed.payer_history.as_ref()
        ));
    }

    #[test]
    fn enabled_section_reads_every_field() {
        let parsed: Stub = toml::from_str(
            "[payer_history]\nenabled = true\nrequire_declaration = true\n\
             experienced_min_trades = 3\nexperienced_min_days = 7",
        )
        .unwrap();
        let cfg = parsed.payer_history.as_ref();
        assert!(PayerHistorySettings::enabled(cfg));
        assert!(PayerHistorySettings::declaration_required(cfg));
        assert_eq!(PayerHistorySettings::experience_thresholds(cfg), (3, 7));
    }
}
