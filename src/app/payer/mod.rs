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
// binary does not call them outside tests.
// TODO(PH-5): remove this attribute once every helper has a caller.
#![allow(dead_code)]

pub mod db;
pub mod declare;

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

const NODE_KEY_ID_DOMAIN: &[u8] = b"mostro-payer-history-node-v1";

/// Fingerprint of the node secret the counterparty ids are keyed with,
/// `sha256("mostro-payer-history-node-v1" ‖ node_secret)` as lowercase hex.
/// The policy row stores it so boot can tell that the secret changed (§10.7).
/// It is the secret, not the public key, that `counterparty_id` hashes, and
/// the x-only public key cannot tell `s` from `n - s`.
pub fn node_key_id(node_keys: &Keys) -> String {
    let mut eng = sha256::Hash::engine();
    eng.input(NODE_KEY_ID_DOMAIN);
    eng.input(node_keys.secret_key().as_secret_bytes());
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

/// `true` when `message` carries a payer hash or a payment history. Such a
/// payload must never reach a log above `debug`: the hash is guessable from
/// a candidate account list, so it is an identifier that only ever travels
/// encrypted between the parties and Mostro (protocol: "The hash is not a
/// secret").
pub fn carries_payer_data(message: &mostro_core::message::Message) -> bool {
    use mostro_core::message::Payload;
    matches!(
        message.get_inner_message_kind().payload,
        Some(Payload::PayerDeclaration(_)) | Some(Payload::PaymentHistory(_))
    )
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
    fn node_key_id_tells_apart_secrets_with_the_same_pubkey() {
        // `s` and `n - s` share the x-only public key but not the secret
        // bytes `counterparty_id` hashes.
        let keys = Keys::generate();
        let negated =
            bitcoin::secp256k1::SecretKey::from_slice(keys.secret_key().as_secret_bytes())
                .unwrap()
                .negate();
        let twin =
            Keys::new(nostr_sdk::prelude::SecretKey::from_slice(&negated.secret_bytes()).unwrap());
        assert_eq!(keys.public_key(), twin.public_key());
        assert_ne!(node_key_id(&keys), node_key_id(&twin));
        assert_eq!(node_key_id(&keys), node_key_id(&keys));
    }

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

    #[test]
    fn payer_payloads_are_flagged_for_log_redaction() {
        use mostro_core::prelude::*;
        let id = Some(uuid::Uuid::new_v4());
        let hash = "a".repeat(64);
        let declared = Message::new_order(
            id,
            None,
            None,
            Action::PayerDeclared,
            Some(Payload::PayerDeclaration(PayerDeclaration::new(
                hash.clone(),
            ))),
        );
        let history = Message::new_order(
            id,
            None,
            None,
            Action::PaymentHistory,
            Some(Payload::PaymentHistory(PaymentHistory::unavailable(hash))),
        );
        assert!(carries_payer_data(&declared));
        assert!(carries_payer_data(&history));

        let other = Message::new_order(id, None, None, Action::FiatSentOk, None);
        assert!(!carries_payer_data(&other));
    }
}
