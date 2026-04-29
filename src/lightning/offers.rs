//! BOLT12 offer and invoice helpers.
//!
//! This module provides format classification for buyer payment requests and
//! semantic validation for BOLT12 offers. It uses the `lightning` crate's
//! `offers` module for pure parsing — no network I/O happens here.
//!
//! Actually paying a BOLT12 offer requires a running LNDK daemon; see
//! [`crate::lightning::lndk`] for the RPC client.

use lightning::offers::offer::{Amount, Offer, Quantity};
use mostro_core::prelude::*;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Classification of a buyer-supplied payment request string.
///
/// Used by [`classify`] and dispatched on in `is_valid_invoice` and
/// `do_payment`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceFormat {
    /// A BOLT11 Lightning invoice (`lnbc…` / `lntb…` / `lnbcrt…`).
    Bolt11,
    /// A BOLT12 offer string (`lno1…`).
    Bolt12Offer,
    /// A pre-fetched BOLT12 invoice (`lni1…`). Not supported in the first
    /// iteration — buyers should send an offer instead.
    Bolt12Invoice,
    /// A Lightning Address (`user@domain.tld`).
    LnAddress,
    /// An LNURL-pay request (`lnurl1…`).
    Lnurl,
    /// Unrecognized format.
    Unknown,
}

/// Classifies a payment request by cheap prefix sniffing.
///
/// Prefix checks are case-insensitive on the HRP and order-sensitive: BOLT12
/// prefixes must come before BOLT11 so `lno1…` is not mis-detected as
/// `ln…`-something.
pub fn classify(payment_request: &str) -> InvoiceFormat {
    let trimmed = payment_request.trim();
    let lower = trimmed.to_ascii_lowercase();

    if lower.starts_with("lno1") {
        return InvoiceFormat::Bolt12Offer;
    }
    if lower.starts_with("lni1") {
        return InvoiceFormat::Bolt12Invoice;
    }
    if lower.starts_with("lnbc")
        || lower.starts_with("lntb")
        || lower.starts_with("lntbs")
        || lower.starts_with("lnbcrt")
    {
        return InvoiceFormat::Bolt11;
    }
    if lower.starts_with("lnurl") {
        return InvoiceFormat::Lnurl;
    }
    // Lightning Address: "user@host" — very loose check, the real validation
    // happens in `lnurl::lightning_address::LightningAddress::from_str`.
    if trimmed.contains('@') && !trimmed.contains(' ') {
        return InvoiceFormat::LnAddress;
    }
    InvoiceFormat::Unknown
}

/// Returns the storage tag written to `orders.buyer_invoice_kind` for a given
/// format. `None` for [`InvoiceFormat::Unknown`] since unknown formats are
/// rejected at validation time.
pub fn kind_tag(fmt: InvoiceFormat) -> Option<&'static str> {
    match fmt {
        InvoiceFormat::Bolt11 => Some("bolt11"),
        InvoiceFormat::Bolt12Offer => Some("bolt12_offer"),
        InvoiceFormat::Bolt12Invoice => Some("bolt12_invoice"),
        InvoiceFormat::LnAddress => Some("lnaddr"),
        InvoiceFormat::Lnurl => Some("lnurl"),
        InvoiceFormat::Unknown => None,
    }
}

/// Validates a BOLT12 offer for acceptance as a buyer payout destination.
///
/// `expected_amount_sats` and `fee_sats` are the order's expected payout
/// amount and Mostro fee; if the offer pins an amount, it must match
/// `expected_amount_sats - fee_sats`.
///
/// Rejects offers that:
/// - Fail to parse as BOLT12 bech32
/// - Are denominated in a non-BTC currency
/// - Have a fixed amount that disagrees with the order
/// - Have elapsed their absolute expiry
/// - Cannot satisfy a quantity of 1
///
/// When `lndk_enabled` is false, rejects unconditionally — a BOLT12 offer
/// cannot be paid without a running LNDK daemon, so accepting one here would
/// guarantee a payout failure later.
pub fn validate_offer(
    offer_str: &str,
    expected_amount_sats: Option<u64>,
    fee_sats: u64,
    lndk_enabled: bool,
) -> Result<(), MostroError> {
    if !lndk_enabled {
        return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
    }

    let offer = Offer::from_str(offer_str)
        .map_err(|_| MostroInternalErr(ServiceError::InvoiceInvalidError))?;

    // Reject non-BTC currency offers — we cannot fetch a satoshi-denominated
    // invoice from them.
    match offer.amount() {
        Some(Amount::Currency { .. }) => {
            return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
        }
        Some(Amount::Bitcoin { amount_msats }) => {
            if let Some(expected) = expected_amount_sats {
                let expected_msats = expected.saturating_sub(fee_sats).saturating_mul(1000);
                // A zero-amount order has nothing to compare — this is already
                // filtered out at payout time, but be defensive.
                if expected_msats != 0 && amount_msats != expected_msats {
                    return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
                }
            }
        }
        None => {
            // Amount-less offer — LNDK will set the amount at fetch time from
            // the caller-supplied `amount_msats`. Nothing to check here.
        }
    }

    // Reject offers whose absolute expiry has elapsed.
    if let Some(expiry) = offer.absolute_expiry() {
        if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
            if expiry <= now {
                return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
            }
        }
    }

    // Reject offers that cannot satisfy a single-item purchase.
    match offer.supported_quantity() {
        Quantity::One | Quantity::Unbounded => {}
        Quantity::Bounded(n) => {
            if n.get() < 1 {
                return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
            }
        }
    }

    Ok(())
}

/// Post-fetch validation of a BOLT12 invoice returned by LNDK's `GetInvoice`.
///
/// Defense-in-depth check: LNDK's `PayOffer` does not verify that the fetched
/// invoice's amount matches what the caller asked for, nor that it has a
/// usable expiry window. We call `GetInvoice` first, run these checks on
/// `Bolt12InvoiceContents`, and only then call `PayInvoice`.
///
/// - `expected_amount_msats` — the amount Mostro asked LNDK to fetch.
/// - `min_expiry_secs` — minimum remaining lifetime before we consider the
///   invoice safe to route. Typically the order's invoice expiration window.
pub fn validate_fetched_invoice(
    amount_msats: u64,
    created_at_unix: i64,
    relative_expiry_secs: u64,
    expected_amount_msats: u64,
    min_expiry_secs: u64,
) -> Result<(), MostroError> {
    if amount_msats != expected_amount_msats {
        return Err(MostroInternalErr(ServiceError::LnPaymentError(format!(
            "BOLT12 invoice amount mismatch: got {amount_msats} msats, expected {expected_amount_msats}"
        ))));
    }

    if created_at_unix < 0 {
        return Err(MostroInternalErr(ServiceError::LnPaymentError(
            "BOLT12 invoice has negative created_at".into(),
        )));
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expires_at = (created_at_unix as u64).saturating_add(relative_expiry_secs);
    if expires_at < now.saturating_add(min_expiry_secs) {
        return Err(MostroInternalErr(ServiceError::LnPaymentError(
            "BOLT12 invoice expired or too close to expiry".into(),
        )));
    }

    // Upper bound on relative expiry: an absurdly long window is suspicious.
    if Duration::from_secs(relative_expiry_secs) > Duration::from_secs(60 * 60 * 24 * 30) {
        return Err(MostroInternalErr(ServiceError::LnPaymentError(
            "BOLT12 invoice relative_expiry exceeds sanity bound".into(),
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_bolt12_offer() {
        assert_eq!(
            classify("lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve2rlrm9deu7xyfzrcgqgn3qzsyvfkx26qkyypvr5hfx60h9w9k934lq8r2n"),
            InvoiceFormat::Bolt12Offer
        );
    }

    #[test]
    fn classify_bolt12_invoice() {
        assert_eq!(classify("lni1qqg…"), InvoiceFormat::Bolt12Invoice);
        assert_eq!(classify("LNI1QQG…"), InvoiceFormat::Bolt12Invoice);
    }

    #[test]
    fn classify_bolt11() {
        assert_eq!(
            classify("lnbc1pvjluezpp5qqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqqqsyqcyq5rqwzqfqypq"),
            InvoiceFormat::Bolt11
        );
        assert_eq!(classify("lnbcrt1p…"), InvoiceFormat::Bolt11);
        assert_eq!(classify("lntb1p…"), InvoiceFormat::Bolt11);
    }

    #[test]
    fn classify_lnurl_and_address() {
        assert_eq!(classify("lnurl1dp68gurn8ghj7…"), InvoiceFormat::Lnurl);
        assert_eq!(classify("alice@getalby.com"), InvoiceFormat::LnAddress);
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(classify(""), InvoiceFormat::Unknown);
        assert_eq!(classify("garbage"), InvoiceFormat::Unknown);
    }

    #[test]
    fn validate_offer_rejects_when_lndk_disabled() {
        // Any string — should bail before parsing.
        let err = validate_offer("lno1anything", None, 0, false).unwrap_err();
        assert!(matches!(
            err,
            MostroInternalErr(ServiceError::InvoiceInvalidError)
        ));
    }

    #[test]
    fn validate_offer_rejects_garbage() {
        let err = validate_offer("lno1not_a_real_offer", None, 0, true).unwrap_err();
        assert!(matches!(
            err,
            MostroInternalErr(ServiceError::InvoiceInvalidError)
        ));
    }

    #[test]
    fn validate_fetched_invoice_amount_mismatch() {
        let err =
            validate_fetched_invoice(1_000_000, 1_700_000_000, 3600, 500_000, 60).unwrap_err();
        match err {
            MostroInternalErr(ServiceError::LnPaymentError(msg)) => {
                assert!(msg.contains("amount mismatch"));
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn validate_fetched_invoice_expired() {
        // Created one hour ago, 60s expiry — well past now.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let err = validate_fetched_invoice(1_000_000, now - 3600, 60, 1_000_000, 60).unwrap_err();
        match err {
            MostroInternalErr(ServiceError::LnPaymentError(msg)) => {
                assert!(msg.contains("expired"));
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn validate_fetched_invoice_accepts_fresh() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(validate_fetched_invoice(1_000_000, now, 3600, 1_000_000, 60).is_ok());
    }

    #[test]
    fn validate_fetched_invoice_rejects_absurd_expiry() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let err = validate_fetched_invoice(
            1_000_000,
            now,
            60 * 60 * 24 * 365, // 1 year
            1_000_000,
            60,
        )
        .unwrap_err();
        match err {
            MostroInternalErr(ServiceError::LnPaymentError(msg)) => {
                assert!(msg.contains("sanity bound"));
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn kind_tags() {
        assert_eq!(kind_tag(InvoiceFormat::Bolt11), Some("bolt11"));
        assert_eq!(kind_tag(InvoiceFormat::Bolt12Offer), Some("bolt12_offer"));
        assert_eq!(kind_tag(InvoiceFormat::Unknown), None);
    }
}
