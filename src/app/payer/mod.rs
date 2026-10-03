//! Payment-sender declaration and payment-account history
//! (anti-triangulation, `docs/PAYER_HISTORY_ANTI_TRIANGULATION.md`).
//!
//! Everything here is **inert** unless `Settings::is_payer_history_enabled()`
//! (or, in handlers, `ctx.settings().payer_history`) says the feature is on.
//! Callers must gate on that flag (D-10).
//!
//! PH-1 delivers only the storage layer ([`db`]); the handlers and the
//! success hook are wired by PH-2 to PH-5.

// The helpers are wired into the trade flow by PH-2 to PH-5; until then the
// binary does not call them outside tests. PH-5 removes this attribute.
#![allow(dead_code)]

pub mod db;

use bitcoin::hashes::{sha256, Hash, HashEngine};
use mostro_core::error::{CantDoReason, MostroError, MostroError::MostroCantDo};
use mostro_core::payer::is_valid_payment_hash;
use nostr_sdk::prelude::Keys;

/// Domain-separation tag for [`counterparty_id`] (D-7).
const COUNTERPARTY_ID_DOMAIN: &[u8] = b"mostro-payer-history-cp-v1";

/// Keyed hash identifying a seller in the counterparty tables (D-7).
///
/// `sha256("mostro-payer-history-cp-v1" ‖ node_secret ‖ seller_master_pubkey)`
/// as lowercase hex. Without the node secret the history tables cannot be
/// joined back to `orders` / `users`, so an exported database does not list
/// which sellers a buyer dealt with.
pub fn counterparty_id(node_keys: &Keys, seller_master_pubkey: &str) -> String {
    let mut eng = sha256::Hash::engine();
    eng.input(COUNTERPARTY_ID_DOMAIN);
    eng.input(node_keys.secret_key().as_secret_bytes());
    eng.input(seller_master_pubkey.as_bytes());
    sha256::Hash::from_engine(eng).to_string()
}

/// Reject a `payment_hash` that is not 64 lowercase hex characters with
/// `cant-do invalid_payment_hash`. Used by the `declare-payer` handler and
/// as a last line of defence by [`db::bump_history`].
pub fn validate_payment_hash(hash: &str) -> Result<(), MostroError> {
    if is_valid_payment_hash(hash) {
        Ok(())
    } else {
        Err(MostroCantDo(CantDoReason::InvalidPaymentHash))
    }
}

/// D-7 qualification predicate: a seller with `qualifying_trades` prior
/// successes, the first at `first_qualifying_at`, is *experienced* at
/// `reference_time` when it meets both thresholds.
pub fn is_experienced(
    experience: &db::SellerExperience,
    min_trades: u32,
    min_days: u32,
    reference_time: i64,
) -> bool {
    experience.qualifying_trades >= min_trades
        && experience
            .first_qualifying_at
            .is_some_and(|t| reference_time - t >= i64::from(min_days) * 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_DAY: i64 = 86_400;

    #[test]
    fn counterparty_id_is_deterministic_per_node_key() {
        let keys = Keys::generate();
        let seller = Keys::generate().public_key().to_string();
        assert_eq!(
            counterparty_id(&keys, &seller),
            counterparty_id(&keys, &seller)
        );
        assert_eq!(counterparty_id(&keys, &seller).len(), 64);
    }

    #[test]
    fn counterparty_id_differs_across_node_keys_and_sellers() {
        let (k1, k2) = (Keys::generate(), Keys::generate());
        let (s1, s2) = (
            Keys::generate().public_key().to_string(),
            Keys::generate().public_key().to_string(),
        );
        assert_ne!(counterparty_id(&k1, &s1), counterparty_id(&k2, &s1));
        assert_ne!(counterparty_id(&k1, &s1), counterparty_id(&k1, &s2));
        // Never the raw pubkey.
        assert_ne!(counterparty_id(&k1, &s1), s1);
    }

    #[test]
    fn validate_payment_hash_maps_to_invalid_payment_hash() {
        assert!(validate_payment_hash(&"a".repeat(64)).is_ok());
        for bad in ["", "abc", &"A".repeat(64), &"z".repeat(64), &"a".repeat(65)] {
            match validate_payment_hash(bad) {
                Err(MostroCantDo(CantDoReason::InvalidPaymentHash)) => {}
                other => panic!("{bad:?} should be rejected, got {other:?}"),
            }
        }
    }

    #[test]
    fn is_experienced_needs_both_thresholds() {
        let now = 1_000 * ONE_DAY;
        let exp = |n, first: Option<i64>| db::SellerExperience {
            qualifying_trades: n,
            first_qualifying_at: first,
        };
        assert!(is_experienced(
            &exp(5, Some(now - 30 * ONE_DAY)),
            5,
            30,
            now
        ));
        // One trade short.
        assert!(!is_experienced(
            &exp(4, Some(now - 30 * ONE_DAY)),
            5,
            30,
            now
        ));
        // One second too young.
        assert!(!is_experienced(
            &exp(5, Some(now - 30 * ONE_DAY + 1)),
            5,
            30,
            now
        ));
        // No qualifying trade at all.
        assert!(!is_experienced(&exp(0, None), 0, 0, now));
    }
}
