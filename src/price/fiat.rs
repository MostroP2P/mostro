//! The §6.6 fiat allowlist.
//!
//! Providers return junk for our purposes — `currency-api` ships **324+**
//! entries including crypto (`eth`, `bnb`, `ada`) and non-ISO codes, and
//! Yadio quotes metals (`XAU`, `XAG`, `XPT`) and BTC itself. The aggregator
//! restricts to the known fiat set below; everything else is dropped before
//! aggregation and before the Nostr publish, keeping the store and the
//! kind-30078 event lean (spec §6.6).
//!
//! The set is **ISO-4217 active codes** plus the non-ISO codes that are
//! nonetheless real, Mostro-traded fiat — all of them present in Yadio's
//! live feed today:
//!
//! - `IRT` — Iranian toman (the everyday unit; `IRR` is the official rial),
//! - `GGP` / `IMP` / `JEP` — Guernsey / Manx / Jersey pounds (GBP-pegged),
//! - `MLC` — Cuban MLC (the spec's motivating fiat-cross case, §11.3).
//!
//! Deliberately **excluded**: `VEF` (the pre-redenomination Venezuelan
//! code some APIs still report) — its scale diverges from the ISO `VES` by
//! orders of magnitude, so letting it in could price an order with a
//! garbage rate. Same reasoning as scoping out `currency-api`'s official
//! CUP: a different unit is worse than no data.
//!
//! Against the live Yadio feed this drops exactly `BTC`, `XAU`, `XAG`,
//! `XPT` — the non-fiat tail — and nothing else, so Phase 2 does not
//! silently remove a currency a node was publishing yesterday.

/// Sorted (binary-searchable) allowlist. Keep alphabetical — there is a
/// test asserting the order so `binary_search` stays correct.
const KNOWN_FIAT: &[&str] = &[
    "AED", "AFN", "ALL", "AMD", "ANG", "AOA", "ARS", "AUD", "AWG", "AZN", "BAM", "BBD", "BDT",
    "BGN", "BHD", "BIF", "BMD", "BND", "BOB", "BRL", "BSD", "BTN", "BWP", "BYN", "BZD", "CAD",
    "CDF", "CHF", "CLP", "CNY", "COP", "CRC", "CUP", "CVE", "CZK", "DJF", "DKK", "DOP", "DZD",
    "EGP", "ERN", "ETB", "EUR", "FJD", "FKP", "GBP", "GEL", "GGP", "GHS", "GIP", "GMD", "GNF",
    "GTQ", "GYD", "HKD", "HNL", "HTG", "HUF", "IDR", "ILS", "IMP", "INR", "IQD", "IRR", "IRT",
    "ISK", "JEP", "JMD", "JOD", "JPY", "KES", "KGS", "KHR", "KMF", "KPW", "KRW", "KWD", "KYD",
    "KZT", "LAK", "LBP", "LKR", "LRD", "LSL", "LYD", "MAD", "MDL", "MGA", "MKD", "MLC", "MMK",
    "MNT", "MOP", "MRU", "MUR", "MVR", "MWK", "MXN", "MYR", "MZN", "NAD", "NGN", "NIO", "NOK",
    "NPR", "NZD", "OMR", "PAB", "PEN", "PGK", "PHP", "PKR", "PLN", "PYG", "QAR", "RON", "RSD",
    "RUB", "RWF", "SAR", "SBD", "SCR", "SDG", "SEK", "SGD", "SHP", "SLE", "SOS", "SRD", "SSP",
    "STN", "SVC", "SYP", "SZL", "THB", "TJS", "TMT", "TND", "TOP", "TRY", "TTD", "TWD", "TZS",
    "UAH", "UGX", "USD", "UYU", "UZS", "VES", "VND", "VUV", "WST", "XAF", "XCD", "XCG", "XOF",
    "XPF", "YER", "ZAR", "ZMW", "ZWL",
];

/// Whether `code` (any casing) is a known fiat currency. Non-fiat codes are
/// dropped at the manager boundary before aggregation (spec §6.6).
pub fn is_known_fiat(code: &str) -> bool {
    let upper = code.to_uppercase();
    KNOWN_FIAT.binary_search(&upper.as_str()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_sorted_and_deduped() {
        // binary_search precondition; a mis-sorted insert would silently
        // make some legitimate currency "unknown".
        let mut sorted = KNOWN_FIAT.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(KNOWN_FIAT, sorted.as_slice());
    }

    #[test]
    fn accepts_major_and_motivating_codes() {
        for code in ["USD", "EUR", "ARS", "CUP", "MLC", "IRT", "VES", "JPY"] {
            assert!(is_known_fiat(code), "{code} must be known fiat");
        }
        // Case-insensitive: currency-api ships lowercase.
        assert!(is_known_fiat("usd"));
        assert!(is_known_fiat("cup"));
    }

    #[test]
    fn rejects_crypto_metals_and_junk() {
        // The §6.6 motivating junk: crypto from currency-api, metals and
        // the BTC self-quote from Yadio.
        // `VEF` is rejected on purpose: pre-redenomination unit, not `VES`.
        for code in [
            "BTC", "ETH", "BNB", "ADA", "XAU", "XAG", "XPT", "1INCH", "VEF", "",
        ] {
            assert!(!is_known_fiat(code), "{code} must be rejected");
        }
    }

    #[test]
    fn live_yadio_codes_survive_except_non_fiat() {
        // Captured from the live Yadio feed 2026-06-11 (128 codes). The
        // allowlist must pass every one of them except the four non-fiat
        // entries — Phase 2 must not silently drop a currency nodes were
        // publishing yesterday.
        let yadio_live = [
            "AED", "ALL", "ANG", "AOA", "ARS", "AUD", "AWG", "AZN", "BAM", "BBD", "BDT", "BGN",
            "BHD", "BIF", "BMD", "BOB", "BRL", "BSD", "BTC", "BTN", "BWP", "BYN", "BZD", "CAD",
            "CDF", "CHF", "CLP", "CNY", "COP", "CRC", "CUP", "CVE", "CZK", "DJF", "DKK", "DOP",
            "DZD", "EGP", "ERN", "ETB", "EUR", "FKP", "GBP", "GEL", "GGP", "GHS", "GIP", "GMD",
            "GNF", "GTQ", "HKD", "HNL", "HUF", "IDR", "ILS", "IMP", "INR", "IRR", "IRT", "ISK",
            "JEP", "JMD", "JOD", "JPY", "KES", "KGS", "KMF", "KRW", "KYD", "KZT", "LBP", "LKR",
            "LSL", "MAD", "MGA", "MLC", "MOP", "MRU", "MWK", "MXN", "MYR", "NAD", "NGN", "NIO",
            "NOK", "NPR", "NZD", "OMR", "PAB", "PEN", "PHP", "PKR", "PLN", "PYG", "QAR", "RON",
            "RSD", "RUB", "RWF", "SAR", "SEK", "SGD", "SHP", "SYP", "SZL", "THB", "TMT", "TND",
            "TRY", "TTD", "TWD", "TZS", "UAH", "UGX", "USD", "UYU", "UZS", "VES", "VND", "XAF",
            "XAG", "XAU", "XCD", "XCG", "XOF", "XPT", "ZAR", "ZMW",
        ];
        let non_fiat = ["BTC", "XAU", "XAG", "XPT"];
        for code in yadio_live {
            if non_fiat.contains(&code) {
                assert!(!is_known_fiat(code), "{code} is non-fiat, must drop");
            } else {
                assert!(is_known_fiat(code), "{code} from live Yadio must pass");
            }
        }
    }
}
